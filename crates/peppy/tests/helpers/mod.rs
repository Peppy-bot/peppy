use peppy::commands::init;

pub fn setup() {
    println!("Setup function called!");

    match init::init() {
        Ok(path) => println!("Initialized peppy.star at: {:?}", path),
        Err(e) => eprintln!("Failed to initialize: {}", e),
    }
}
