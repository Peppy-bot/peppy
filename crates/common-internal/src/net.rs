use std::sync::atomic::{AtomicU32, Ordering};

const PORT_START: u16 = 40_000;
const PORT_END: u16 = 65_000;
static NEXT_PORT: AtomicU32 = AtomicU32::new(PORT_START as u32);

pub fn pick_free_tcp_port() -> u16 {
    loop {
        let current = NEXT_PORT.load(Ordering::Relaxed);
        let candidate = if current >= PORT_END as u32 {
            PORT_START as u32
        } else {
            current
        };
        let next = candidate + 1;
        if NEXT_PORT
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return candidate as u16;
        }
    }
}
