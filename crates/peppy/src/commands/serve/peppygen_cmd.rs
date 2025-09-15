use super::{ServeAsyncCommand, ServeFuture};
use crate::InterfacesGenerator;

impl ServeAsyncCommand for InterfacesGenerator {
    fn run(&self) -> ServeFuture {
        Box::pin(async {
            let _gen = InterfacesGenerator::new();
            Ok(())
        })
    }
}
