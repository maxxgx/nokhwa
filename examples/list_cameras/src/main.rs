//! Run with `cargo run -p list_cameras`.
use nokhwa::utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::{native_api_backend, query, Camera};

fn main() {
    let backend = native_api_backend().unwrap_or(ApiBackend::Auto);
    println!("using backend: {backend:?}\n");

    let cameras = match query(backend) {
        Ok(cameras) => cameras,
        Err(e) => {
            eprintln!("failed to query cameras: {e}");
            std::process::exit(1);
        }
    };

    if cameras.is_empty() {
        println!("no cameras found");
        return;
    }

    println!("found {} camera(s)\n", cameras.len());

    for info in &cameras {
        // if !can_open(info.index().clone()){
        //     continue;
        // }
        println!("================================================================");
        println!("index:            `{}`", info.index());
        println!("name:             `{}`", info.human_name());
        println!("description:      `{}`", info.description());
        println!("device_meta_json: `{}`", info.device_meta_json());


    }
}

fn can_open(index: CameraIndex) -> bool {
    let none_requested = RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
        RequestedFormatType::None,
    );

    let res = match Camera::new(index.clone(), none_requested) {
        Ok(_) => true,
        Err(_) => {
            let highest_res_requested = RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
                RequestedFormatType::AbsoluteHighestResolution,
            );
            match Camera::new(index, highest_res_requested) {
                Ok(_) => true,
                Err(_) => false

            }
        }
    };
    res
}
