use peppygen::{NodeBuilder, Parameters, Result};
use peppylib::runtime::StandaloneConfig;

fn main() -> Result<()> {
    // Parameters can also be defined directly in code:
    //
    // use peppygen::parameters::{device::Device, video::{Video, VideoResolution}};
    //
    // let params = Parameters {
    //     device: Device {
    //         physical: "/dev/video0".to_string(),
    //         sim: "virtual_camera".to_string(),
    //         priority: "high".to_string(),
    //     },
    //     video: Video {
    //         frame_rate: 30,
    //         resolution: VideoResolution {
    //             width: 1920,
    //             height: 1080,
    //         },
    //         encoding: "h264".to_string(),
    //     },
    // };

    let json = std::fs::read_to_string("params.json")
        .expect("failed to read params.json");
    let params: Parameters = serde_json::from_str(&json)
        .expect("failed to parse params.json");

    let standalone_config = StandaloneConfig::new().with_parameters(&params);
    NodeBuilder::new()
        .standalone(standalone_config)
        .run(|args: Parameters, node_runner| async {
            println!("Inside the run closure!");
            let _ = args;
            let _ = node_runner;
            Ok(())
        })
}
