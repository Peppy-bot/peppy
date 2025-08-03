// The Messenger is an abstraction of a PubSub system such as Zenoh
pub trait Messenger {
    fn send(&self, msg: &str);
}

pub struct PubSub<'a, T: Messenger> {
    messenger: &'a T,
}

impl<'a, T> PubSub<'a, T>
where
    T: Messenger,
{
    pub fn new(messenger: &'a T, max: usize) -> PubSub<'a, T> {
        PubSub {
            messenger,
        }
    }
}