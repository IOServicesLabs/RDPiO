//! The desktop's allocation counters, for `metrics`: not kept on Android
//! (the system allocator is used as is).

pub fn drain() -> (u64, u64) {
    (0, 0)
}
