use peppygen::{
    NodeBuilder, Parameters, Result,
    parameters::{
        device::Device,
        video::{Resolution, Video},
    },
};
use peppylib::runtime::StandaloneConfig;

fn main() -> Result<()> {
    // Those arguments could eventually be parsed with clap
    let fake_params = Parameters {
        device: Device {
            physical: "/dev/video0".to_string(),
            sim: "virtual_camera".to_string(),
            priority: "high".to_string(),
        },
        video: Video {
            frame_rate: 30,
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
            encoding: "h264".to_string(),
        },
    };

    let standalone_config = StandaloneConfig::new().with_parameters(&fake_params);
    NodeBuilder::new()
        .standalone(standalone_config)
        .run(|args: Parameters, node_runner| async {
            println!("Inside the run closure!");
            let _ = args;
            let _ = node_runner;
            Ok(())
        })
}
