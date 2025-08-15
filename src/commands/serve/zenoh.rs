struct Zenoh {
    sent_messages: Vec<String>,
}

impl Zenoh {
    fn new() -> Zenoh {
        Zenoh {
            sent_messages: vec![],
        }
    }
}

impl Messenger for Zenoh {
    fn send(&self, message: &str) {
        self.sent_messages.push(String::from(message));
    }
}
