// use peppylib;
// use peppygen; // peppygen exposes generated interface

// #[peppylib::main]
// fn main() -> ! {
//     println!("Hello, world!");
// }

// use peppylib; // control center
// use peppygen; // peppygen exposes generated interface

// #[tokio::main]
// async fn main() -> Result<(), Box<dyn std::error::Error>> {
//     // Setup the node
//     let node = peppylib::setup_node().await?;

//     // Create a subscription on the node for a topic, providing an async callback
//     let subscription = node
//         .create_subscription("topic_name", |msg| {
//             async move {
//                 println!("Received message: {:?}", msg);
//                 // Process the message asynchronously here
//             }
//         })
//         .await?;

//     // Keep running until Ctrl+C
//     tokio::signal::ctrl_c().await?;
//     println!("Shutdown signal received, exiting...");
//     Ok(())
// }

fn main() {}
