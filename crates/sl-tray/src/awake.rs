//! Keeps the Mac from idle-sleeping while a Claude Code session is working.
//!
//! Runs `caffeinate -i -w <our pid>` for as long as at least one open session
//! is busy and "Auto caffeinate" is on (the switch in the menu), or one is in
//! `/rc`, and kills it when neither holds. `-w` makes `caffeinate` exit
//! on its own if the tray dies, so a crash cannot leave the Mac awake for good.
//! `-i` stops idle sleep only: the display may still sleep, and closing the lid
//! still sleeps the machine.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use objc2_foundation::NSString;

/// Whether "Auto caffeinate" is on. Written by the menu's switch, read by the
/// polling thread. Kept across restarts by [`load`] and [`save`].
pub type AutoCaffeinate = Arc<AtomicBool>;

const DEFAULT_KEY: &str = "autoCaffeinate";

/// The saved switch. On until someone turns it off: a switch that has never
/// been touched has no stored value, which is not the same as `false`.
pub fn load() -> bool {
    let defaults = crate::view_prefs::defaults();
    let key = NSString::from_str(DEFAULT_KEY);
    defaults
        .objectForKey(&key)
        .is_none_or(|_| defaults.boolForKey(&key))
}

/// Remember the switch for the next launch.
pub fn save(on: bool) {
    crate::view_prefs::defaults().setBool_forKey(on, &NSString::from_str(DEFAULT_KEY));
}

#[derive(Default)]
pub struct KeepAwake {
    child: Option<Child>,
}

impl KeepAwake {
    /// Bring `caffeinate` in line with whether any session is busy. Called on
    /// every poll, so a `caffeinate` that died is started again.
    pub fn set(&mut self, busy: bool) {
        if let Some(child) = &mut self.child
            && !matches!(child.try_wait(), Ok(None))
        {
            self.child = None;
        }
        match (busy, self.child.is_some()) {
            (true, false)
                if crate::effects::intercept(crate::effects::Effect::Caffeinate(true)) => {}
            (true, false) => {
                let spawned = Command::new("/usr/bin/caffeinate")
                    .args(["-i", "-w", &std::process::id().to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                match spawned {
                    Ok(child) => {
                        crate::trace("caffeinate: started");
                        self.child = Some(child);
                    }
                    Err(error) => crate::log(&format!("could not run caffeinate: {error}")),
                }
            }
            (false, true) => self.release(),
            _ => {}
        }
    }

    fn release(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().ok();
            child.wait().ok();
            crate::trace("caffeinate: stopped");
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caffeinate_runs_only_while_busy() {
        let mut awake = KeepAwake::default();
        awake.set(false);
        assert!(awake.child.is_none());
        awake.set(true);
        let pid = awake.child.as_ref().expect("started").id();
        awake.set(true);
        assert_eq!(
            awake.child.as_ref().map(Child::id),
            Some(pid),
            "a second busy poll reuses it"
        );
        awake.set(false);
        assert!(awake.child.is_none());
    }
}
