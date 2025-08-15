use peppy::commands::init;

pub fn setup() {
    println!("Setup function called!");
    let current_dir = std::env::current_dir().expect("Failed to get current directory");

    match init::init(&current_dir) {
        Ok(path) => println!("Initialized peppy.star at: {:?}", path),
        Err(e) => eprintln!("Failed to initialize: {}", e),
    }
}
