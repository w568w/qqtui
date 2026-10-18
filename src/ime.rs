use std::time::Duration;
use tokio::sync::watch;

// Switching belongs to the terminal's desktop, never to a remote SSH host.
pub struct InputMethod {
    sender: watch::Sender<(bool, bool)>,
    done: tokio::sync::oneshot::Receiver<()>,
}
impl InputMethod {
    pub fn start(enabled: bool, ascii: Option<String>) -> Option<Self> {
        if !enabled || std::env::var_os("SSH_CONNECTION").is_some() {
            return None;
        }
        let (sender, mut receiver) = watch::channel((false, false));
        let (done_tx, done) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("qqtui-ime".into())
            .spawn(move || {
                if let Some(mut state) = Native::detect(ascii) {
                    while futures::executor::block_on(receiver.changed()).is_ok() {
                        let (active, text) = *receiver.borrow_and_update();
                        if active {
                            state.update(text);
                        }
                    }
                    if receiver.borrow().0 {
                        state.update(true);
                    }
                }
                let _ = done_tx.send(());
            })
            .ok()?;
        Some(Self { sender, done })
    }
    pub fn update(&self, focused: bool, text: bool) {
        self.sender.send_if_modified(|old| {
            if *old == (focused, text) {
                return false;
            }
            *old = (focused, text);
            true
        });
    }
    pub async fn finish(self) {
        drop(self.sender);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.done).await;
    }
}

trait Backend {
    fn get(&self) -> Option<String>;
    fn set(&self, value: &str) -> bool;
}
struct Switch<B> {
    backend: B,
    ascii: String,
    saved: Option<String>,
    forced: bool,
}
impl<B: Backend> Switch<B> {
    fn update(&mut self, text: bool) {
        let Some(current) = self.backend.get() else {
            return;
        };
        if text {
            if self.forced
                && current == self.ascii
                && let Some(saved) = self.saved.as_ref()
                && !self.backend.set(saved)
            {
                return;
            }
            self.forced = false;
        } else if !self.forced || current != self.ascii {
            self.saved = Some(current.clone());
            if current != self.ascii && !self.backend.set(&self.ascii) {
                return;
            }
            self.forced = true;
        }
    }
}
struct Native;
impl Native {
    fn detect(_ascii: Option<String>) -> Option<Switch<Self>> {
        #[cfg(windows)]
        let ascii = {
            im_switch::get_ime_state().ok()?;
            "false".to_owned()
        };
        #[cfg(not(windows))]
        let ascii = {
            let methods = im_switch::list_input_methods().ok()?;
            _ascii
                .or_else(|| {
                    methods
                        .iter()
                        .find(|id| {
                            matches!(
                                id.as_str(),
                                "keyboard-us"
                                    | "xkb:us::eng"
                                    | "com.apple.keylayout.ABC"
                                    | "com.apple.keylayout.US"
                            )
                        })
                        .cloned()
                })
                .filter(|candidate| methods.contains(candidate))?
        };
        Some(Switch {
            backend: Native,
            ascii,
            saved: None,
            forced: false,
        })
    }
}
impl Backend for Native {
    fn get(&self) -> Option<String> {
        #[cfg(windows)]
        {
            im_switch::get_ime_state().ok().map(|v| v.to_string())
        }
        #[cfg(not(windows))]
        {
            im_switch::get_input_method().ok()
        }
    }
    fn set(&self, value: &str) -> bool {
        #[cfg(windows)]
        {
            im_switch::set_ime_state(value == "true").is_ok()
        }
        #[cfg(not(windows))]
        {
            im_switch::set_input_method(value).is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Fake {
        current: RefCell<Option<String>>,
        writes: RefCell<Vec<String>>,
        fail: Cell<bool>,
    }
    impl Backend for Fake {
        fn get(&self) -> Option<String> {
            self.current.borrow().clone()
        }
        fn set(&self, value: &str) -> bool {
            assert!(
                self.current.borrow().is_some(),
                "must not write without a readable state"
            );
            if self.fail.get() {
                return false;
            }
            *self.current.borrow_mut() = Some(value.into());
            self.writes.borrow_mut().push(value.into());
            true
        }
    }
    fn fixture() -> Switch<Fake> {
        Switch {
            backend: Fake {
                current: RefCell::new(Some("pinyin".into())),
                writes: RefCell::default(),
                fail: Cell::new(false),
            },
            ascii: "keyboard".into(),
            saved: None,
            forced: false,
        }
    }
    #[test]
    fn an_unavailable_backend_does_not_change_the_saved_state() {
        let mut s = fixture();
        s.saved = Some("pinyin".into());
        s.forced = true;
        *s.backend.current.borrow_mut() = None;
        s.update(false);
        s.update(true);
        assert_eq!(s.saved.as_deref(), Some("pinyin"));
    }
    #[test]
    fn failed_switching_keeps_the_original_method_for_retry() {
        let mut s = fixture();
        s.backend.fail.set(true);
        s.update(false);
        assert!(!s.forced);
        s.backend.fail.set(false);
        s.update(false);
        assert!(s.forced);
        assert_eq!(s.saved.as_deref(), Some("pinyin"));
    }
    #[test]
    fn normal_mode_restores_the_previous_input_method_once() {
        let mut s = fixture();
        s.update(false);
        s.update(false);
        s.update(true);
        s.update(true);
        assert_eq!(*s.backend.writes.borrow(), ["keyboard", "pinyin"]);
        s.update(false);
        *s.backend.current.borrow_mut() = Some("user-choice".into());
        s.update(true);
        assert_eq!(s.backend.current.borrow().as_deref(), Some("user-choice"));
    }
}
