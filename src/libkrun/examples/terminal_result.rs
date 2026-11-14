use krun::VmExit;

pub fn terminal_matches(exit: &VmExit, reason: u8) -> bool {
    matches!(
        (reason, exit),
        (1, VmExit::PowerOffRequested)
            | (2, VmExit::RebootRequested)
            | (3, VmExit::Halted)
            | (4, VmExit::Panic)
    )
}
pub fn smoke_matches(exit: &VmExit, watchdog: bool) -> bool {
    matches!(
        (exit, watchdog),
        (VmExit::DeviceWatchdog, true) | (VmExit::Fault | VmExit::PowerOffRequested, false)
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn result_values_match_shared_contract() {
        for (reason, exit) in [
            (1, VmExit::PowerOffRequested),
            (2, VmExit::RebootRequested),
            (3, VmExit::Halted),
            (4, VmExit::Panic),
        ] {
            assert!(terminal_matches(&exit, reason));
            assert!(!terminal_matches(&VmExit::Fault, reason));
        }
        assert!(smoke_matches(&VmExit::Fault, false));
        assert!(smoke_matches(&VmExit::PowerOffRequested, false));
        assert!(smoke_matches(&VmExit::DeviceWatchdog, true));
        assert!(!smoke_matches(&VmExit::Stopped, false));
    }
}
fn main() {}
