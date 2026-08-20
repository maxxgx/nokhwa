//! Media controller lookups for Linux camera devices.
//!
//! `/dev/v4l/by-id` only exists for cameras that expose a USB serial, so CSI /
//! MIPI sensors (Raspberry Pi cameras, most SoC ISPs) have no stable path there.
//! Those devices are always part of a media-controller graph though, so we can
//! walk the graph backwards from the `/dev/videoN` capture entity until we reach
//! the sensor and use its entity name (`"imx708 10-001a"`, which encodes the I2C
//! bus and address) as a stable identifier instead.
//!
//! The bits of `<linux/media.h>` we need are declared here rather than pulled in
//! from a `-sys` crate: the v2 topology ABI is frozen, and vendoring it avoids a
//! bindgen/libclang dependency in the wheel build.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::CStr,
    fs::{self, File},
    io, mem,
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
};

/*
 * ABI declarations, from <linux/media.h>.
 *
 * Every field in the v2 structs is naturally aligned, so plain repr(C) matches
 * the kernel's __attribute__((packed)) layout without unaligned field reads.
 */

#[repr(C)]
#[derive(Clone, Copy)]
struct MediaV2Topology {
    topology_version: u64,

    num_entities: u32,
    reserved1: u32,
    ptr_entities: u64,

    num_interfaces: u32,
    reserved2: u32,
    ptr_interfaces: u64,

    num_pads: u32,
    reserved3: u32,
    ptr_pads: u64,

    num_links: u32,
    reserved4: u32,
    ptr_links: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MediaV2Entity {
    id: u32,
    name: [libc::c_char; 64],
    function: u32,
    flags: u32,
    reserved: [u32; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MediaV2IntfDevnode {
    major: u32,
    minor: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union MediaV2InterfaceDevnode {
    devnode: MediaV2IntfDevnode,
    raw: [u32; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MediaV2Interface {
    id: u32,
    intf_type: u32,
    flags: u32,
    reserved: [u32; 9],
    devnode: MediaV2InterfaceDevnode,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MediaV2Pad {
    id: u32,
    entity_id: u32,
    flags: u32,
    index: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MediaV2Link {
    id: u32,
    source_id: u32,
    sink_id: u32,
    flags: u32,
    reserved: [u32; 6],
}

/// `_IOWR(type, nr, size)`, as defined by `<asm-generic/ioctl.h>`.
const fn iowr(ty: u32, nr: u32, size: u32) -> u32 {
    const DIR_READ_WRITE: u32 = 3;

    (DIR_READ_WRITE << 30) | (size << 16) | (ty << 8) | nr
}

const MEDIA_IOC_G_TOPOLOGY: u32 = iowr(
    b'|' as u32,
    0x04,
    mem::size_of::<MediaV2Topology>() as u32,
);

const MEDIA_INTF_T_V4L_VIDEO: u32 = 0x0000_0200;
const MEDIA_ENT_F_CAM_SENSOR: u32 = 0x0002_0001;

const MEDIA_LNK_FL_ENABLED: u32 = 1 << 0;
const MEDIA_LNK_FL_LINK_TYPE: u32 = 0xf000_0000;
const MEDIA_LNK_FL_DATA_LINK: u32 = 0 << 28;
const MEDIA_LNK_FL_INTERFACE_LINK: u32 = 1 << 28;

#[derive(Debug, Clone)]
struct Entity {
    name: String,
    function: u32,
}

#[derive(Debug, Clone)]
struct Pad {
    entity_id: u32,
}

#[derive(Debug, Clone)]
struct Link {
    source_id: u32,
    sink_id: u32,
    flags: u32,
}

#[derive(Debug, Clone)]
struct VideoMapping {
    major: u32,
    minor: u32,
    entity_id: u32,
}

#[derive(Debug)]
struct Topology {
    entities: HashMap<u32, Entity>,
    pads: HashMap<u32, Pad>,
    links: Vec<Link>,
    videos: Vec<VideoMapping>,
}

fn c_name(bytes: &[libc::c_char]) -> String {
    let ptr = bytes.as_ptr();
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn load_topology(path: &Path) -> io::Result<Topology> {
    let file = File::open(path)?;
    let fd = file.as_raw_fd();

    /*
     * First ioctl:
     * ask kernel how many objects exist.
     */
    let mut topo: MediaV2Topology = unsafe { mem::zeroed() };

    let ret = unsafe {
        libc::ioctl(fd, MEDIA_IOC_G_TOPOLOGY as _, &mut topo)
    };

    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut entities: Vec<MediaV2Entity> =
        Vec::with_capacity(topo.num_entities as usize);

    let mut interfaces: Vec<MediaV2Interface> =
        Vec::with_capacity(topo.num_interfaces as usize);

    let mut pads: Vec<MediaV2Pad> =
        Vec::with_capacity(topo.num_pads as usize);

    let mut links: Vec<MediaV2Link> =
        Vec::with_capacity(topo.num_links as usize);

    /*
     * The kernel ABI takes userspace pointers encoded as u64.
     */
    topo.ptr_entities = entities.as_mut_ptr() as usize as u64;
    topo.ptr_interfaces = interfaces.as_mut_ptr() as usize as u64;
    topo.ptr_pads = pads.as_mut_ptr() as usize as u64;
    topo.ptr_links = links.as_mut_ptr() as usize as u64;

    let ret = unsafe {
        libc::ioctl(fd, MEDIA_IOC_G_TOPOLOGY as _, &mut topo)
    };

    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    /*
     * The kernel only ever fills in as many objects as it announced in the
     * sizing call, so the capacities above are still valid here.
     */
    unsafe {
        entities.set_len(topo.num_entities as usize);
        interfaces.set_len(topo.num_interfaces as usize);
        pads.set_len(topo.num_pads as usize);
        links.set_len(topo.num_links as usize);
    }

    let entities_map = entities
        .iter()
        .map(|e| {
            (
                e.id,
                Entity {
                    name: c_name(&e.name),
                    function: e.function,
                },
            )
        })
        .collect();

    let pads_map = pads
        .iter()
        .map(|p| {
            (
                p.id,
                Pad {
                    entity_id: p.entity_id,
                },
            )
        })
        .collect::<HashMap<_, _>>();

    let graph_links = links
        .iter()
        .map(|l| Link {
            source_id: l.source_id,
            sink_id: l.sink_id,
            flags: l.flags,
        })
        .collect::<Vec<_>>();

    /*
     * Interface links have:
     *
     *   source_id = interface ID
     *   sink_id   = entity ID
     *
     * whereas data links connect pad IDs.
     *
     * Build interface ID -> entity ID first.
     */
    let mut interface_entity = HashMap::<u32, u32>::new();

    for link in &graph_links {
        let link_type = link.flags & MEDIA_LNK_FL_LINK_TYPE;

        if link_type == MEDIA_LNK_FL_INTERFACE_LINK {
            interface_entity.insert(link.source_id, link.sink_id);
        }
    }

    let mut videos = Vec::new();

    for intf in &interfaces {
        if intf.intf_type != MEDIA_INTF_T_V4L_VIDEO {
            continue;
        }

        let Some(&entity_id) = interface_entity.get(&intf.id) else {
            continue;
        };

        let devnode = unsafe { intf.devnode.devnode };

        videos.push(VideoMapping {
            major: devnode.major,
            minor: devnode.minor,
            entity_id,
        });
    }

    Ok(Topology {
        entities: entities_map,
        pads: pads_map,
        links: graph_links,
        videos,
    })
}

fn find_sensor(topology: &Topology, start_entity: u32) -> Option<&Entity> {
    /*
     * Convert data links:
     *
     * source pad -> sink pad
     *
     * into:
     *
     * source entity -> sink entity
     *
     * We then traverse BACKWARDS from the video capture entity.
     */
    let mut upstream = HashMap::<u32, Vec<u32>>::new();

    for link in &topology.links {
        let link_type = link.flags & MEDIA_LNK_FL_LINK_TYPE;

        if link_type != MEDIA_LNK_FL_DATA_LINK {
            continue;
        }

        /*
         * Only follow links that are currently active. A CSI receiver or ISP
         * usually has data links to several capture nodes with just one of
         * them enabled, so ignoring this flag would make every /dev/videoN in
         * the graph resolve to the same sensor.
         */
        if link.flags & MEDIA_LNK_FL_ENABLED == 0 {
            continue;
        }

        let Some(src_pad) = topology.pads.get(&link.source_id) else {
            continue;
        };

        let Some(dst_pad) = topology.pads.get(&link.sink_id) else {
            continue;
        };

        upstream
            .entry(dst_pad.entity_id)
            .or_default()
            .push(src_pad.entity_id);
    }

    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();

    queue.push_back(start_entity);

    while let Some(entity_id) = queue.pop_front() {
        if !visited.insert(entity_id) {
            continue;
        }

        let Some(entity) = topology.entities.get(&entity_id) else {
            continue;
        };

        if entity.function == MEDIA_ENT_F_CAM_SENSOR {
            return Some(entity);
        }

        if let Some(previous) = upstream.get(&entity_id) {
            queue.extend(previous.iter().copied());
        }
    }

    None
}

// libc exposes major()/minor() as safe fns on some targets and unsafe on others.
#[allow(unused_unsafe)]
fn dev_major_minor(path: &Path) -> io::Result<(u32, u32)> {
    let metadata = fs::metadata(path)?;
    let rdev = metadata.rdev() as libc::dev_t;

    let major = unsafe { libc::major(rdev) };
    let minor = unsafe { libc::minor(rdev) };

    Ok((major as u32, minor as u32))
}

fn media_devices() -> io::Result<Vec<PathBuf>> {
    let mut result = Vec::new();

    for entry in fs::read_dir("/dev")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if let Some(n) = name.strip_prefix("media") {
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
                result.push(entry.path());
            }
        }
    }

    result.sort();

    Ok(result)
}

/*
 * v4l2_capability, from <linux/videodev2.h>. `card` and `bus_info` are
 * fixed-size ASCII byte arrays that may fill the whole field with no
 * trailing NUL, so they're clamped to length rather than read as a CStr.
 */
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `_IOR(type, nr, size)`, as defined by `<asm-generic/ioctl.h>`.
const fn ior(ty: u32, nr: u32, size: u32) -> u32 {
    const DIR_READ: u32 = 2;

    (DIR_READ << 30) | (size << 16) | (ty << 8) | nr
}

const VIDIOC_QUERYCAP: u32 = ior(b'V' as u32, 0, mem::size_of::<V4l2Capability>() as u32);

fn bytes_to_string(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let text = String::from_utf8_lossy(&bytes[..end]).trim().to_string();

    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn query_caps(index: u32) -> io::Result<V4l2Capability> {
    let path = PathBuf::from(format!("/dev/video{index}"));
    let file = File::open(&path)?;

    let mut caps: V4l2Capability = unsafe { mem::zeroed() };

    let ret = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP as _, &mut caps) };

    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(caps)
}

/// Walks up from `/sys/class/video4linux/video{index}/device` — typically a
/// USB *interface* node (e.g. `.../1-3:1.0`) — to the USB *device* node
/// (e.g. `.../1-3`) that carries `idVendor`/`idProduct`/`manufacturer`/
/// `product`/`serial`. Returns `None` for non-USB devices (CSI/MIPI
/// sensors), which have no such ancestor.
fn find_usb_device_dir(index: u32) -> Option<PathBuf> {
    let device_link = PathBuf::from(format!("/sys/class/video4linux/video{index}/device"));
    let mut dir = fs::canonicalize(&device_link).ok()?;

    loop {
        if dir.join("idVendor").is_file() {
            return Some(dir);
        }

        if !dir.pop() || dir == Path::new("/sys") {
            return None;
        }
    }
}

fn read_sysfs_attr(dir: &Path, name: &str) -> Option<String> {
    let text = fs::read_to_string(dir.join(name)).ok()?;
    let text = text.trim();

    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Replicates udev's `udev_replace_whitespace()` + `udev_replace_chars()`
/// well enough for the common case: runs of whitespace or punctuation
/// outside `[A-Za-z0-9#+-.:=@]` collapse to a single `_`, with no leading,
/// trailing, or doubled `_`.
fn encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());

    for ch in raw.trim().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '#' | '+' | '-' | '.' | ':' | '=' | '@') {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }

    out.trim_end_matches('_').to_string()
}

pub fn device_serial(index: u32) -> Option<String> {
    let dir = find_usb_device_dir(index)?;

    let serial = read_sysfs_attr(&dir, "serial")
        .map(|s| encode_component(&s))
        .unwrap_or(String::new());

    Some(serial)
}


/// Bus location for `/dev/video{index}`, from the V4L2 `bus_info` field
/// (e.g. `"usb-0000:00:14.0-3"` for USB, `"platform:..."` for CSI/SoC
/// sensors). Distinguishes otherwise-identical devices by where they're
/// physically attached; unlike `sensor_name`, it needs no media-controller
/// graph and is available for USB webcams too.
pub fn device_bus(index: u32) -> Option<String> {
    bytes_to_string(&query_caps(index).ok()?.bus_info)
}

/// Name of the camera sensor feeding `/dev/video{index}`, according to the
/// media-controller graph the device belongs to.
///
/// Returns e.g. `Some("imx708 10-001a")` for a CSI camera, or `None` when the
/// device is not part of a media graph (most USB webcams) or when no sensor
/// entity is reachable from its capture entity.
pub fn sensor_name(index: u32) -> Option<String> {
    let video = PathBuf::from(format!("/dev/video{index}"));
    let (major, minor) = dev_major_minor(&video).ok()?;

    for media_dev in media_devices().unwrap_or_default() {
        let Ok(topology) = load_topology(&media_dev) else {
            continue;
        };

        let Some(mapping) = topology
            .videos
            .iter()
            .find(|v| v.major == major && v.minor == minor)
        else {
            continue;
        };

        if let Some(sensor) = find_sensor(&topology, mapping.entity_id) {
            return Some(sensor.name.clone().replace(" ","/"));
        }
    }

    None
}
