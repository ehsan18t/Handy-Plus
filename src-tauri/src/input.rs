use enigo::{Enigo, Key, Keyboard, Mouse, Settings};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};

#[cfg(target_os = "macos")]
mod macos {
    use super::Key;
    use log::{debug, warn};
    use std::ffi::c_void;

    type TisInputSourceRef = *const c_void;
    type CfDataRef = *const c_void;
    type CfStringRef = *const c_void;

    // kVK_ANSI_V. This is the behavior Handy used before layout-aware
    // resolution and remains the safest fallback if macOS cannot expose the
    // active layout.
    const ANSI_V_KEYCODE: u16 = 9;
    const KEYCODE_COUNT: u16 = 128;
    const UC_KEY_ACTION_DISPLAY: u16 = 3;
    const UC_KEY_TRANSLATE_NO_DEAD_KEYS_MASK: u32 = 1;
    // Carbon's cmdKey is bit 8. UCKeyTranslate expects Carbon modifiers shifted
    // right by 8, so Command is represented by bit 0 here.
    const COMMAND_MODIFIER_STATE: u32 = 1;

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        fn TISCopyCurrentKeyboardLayoutInputSource() -> TisInputSourceRef;
        fn TISGetInputSourceProperty(
            input_source: TisInputSourceRef,
            property_key: CfStringRef,
        ) -> CfDataRef;
        static kTISPropertyUnicodeKeyLayoutData: CfStringRef;
        fn UCKeyTranslate(
            key_layout: *const u8,
            virtual_key_code: u16,
            key_action: u16,
            modifier_key_state: u32,
            keyboard_type: u32,
            key_translate_options: u32,
            dead_key_state: *mut u32,
            max_string_length: usize,
            actual_string_length: *mut usize,
            unicode_string: *mut u16,
        ) -> i32;
        fn LMGetKbdType() -> u8;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataGetBytePtr(data: CfDataRef) -> *const u8;
        fn CFRelease(value: *const c_void);
    }

    struct InputSource(TisInputSourceRef);

    impl Drop for InputSource {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: TISCopyCurrentKeyboardLayoutInputSource returned this
                // retained reference, so this balances that ownership.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn find_keycode(mut matches: impl FnMut(u16) -> bool) -> Option<u16> {
        (0..KEYCODE_COUNT).find(|&keycode| matches(keycode))
    }

    /// Resolves the physical key that macOS interprets as `v` while Command is
    /// held. Including Command is important: non-Latin layouts commonly map
    /// Cmd shortcuts to their ANSI equivalents, while standard Dvorak does not.
    ///
    /// TIS APIs must run on the main thread. Handy's paste path already enters
    /// through `AppHandle::run_on_main_thread` before reaching this function.
    fn resolve_command_v_keycode() -> Result<u16, String> {
        // SAFETY: This function is called on the macOS main thread. The returned
        // source follows the Create Rule and is released by InputSource::drop.
        let source = InputSource(unsafe { TISCopyCurrentKeyboardLayoutInputSource() });
        if source.0.is_null() {
            return Err("macOS returned no current keyboard layout input source".into());
        }

        // SAFETY: The source remains retained for the duration of the scan and
        // the property constant is provided by Carbon.
        let layout_data =
            unsafe { TISGetInputSourceProperty(source.0, kTISPropertyUnicodeKeyLayoutData) };
        if layout_data.is_null() {
            return Err("current macOS keyboard layout has no Unicode layout data".into());
        }

        // SAFETY: layout_data is a CFData owned by the retained input source and
        // remains valid until source is dropped after the scan.
        let layout = unsafe { CFDataGetBytePtr(layout_data) };
        if layout.is_null() {
            return Err("current macOS keyboard layout data is empty".into());
        }

        // SAFETY: LMGetKbdType has no arguments and returns the current physical
        // keyboard type used by UCKeyTranslate.
        let keyboard_type = unsafe { LMGetKbdType() } as u32;
        let keycode = find_keycode(|keycode| {
            let mut dead_key_state = 0;
            let mut chars = [0_u16; 4];
            let mut length = 0_usize;

            // SAFETY: layout points to valid UCKeyboardLayout bytes while source
            // is retained. All output pointers reference initialized local
            // storage of the declared sizes.
            let status = unsafe {
                UCKeyTranslate(
                    layout,
                    keycode,
                    UC_KEY_ACTION_DISPLAY,
                    COMMAND_MODIFIER_STATE,
                    keyboard_type,
                    UC_KEY_TRANSLATE_NO_DEAD_KEYS_MASK,
                    &mut dead_key_state,
                    chars.len(),
                    &mut length,
                    chars.as_mut_ptr(),
                )
            };

            status == 0 && length == 1 && chars[0] == u16::from(b'v')
        })
        .ok_or_else(|| "could not map Cmd+V in the current macOS keyboard layout".to_string())?;

        Ok(keycode)
    }

    pub(super) fn command_v_key() -> Key {
        match resolve_command_v_keycode() {
            Ok(keycode) => {
                debug!("Resolved Cmd+V for the active macOS layout to keycode {keycode}");
                Key::Other(u32::from(keycode))
            }
            Err(error) => {
                warn!(
                    "Could not resolve Cmd+V for the active macOS layout ({error}); using ANSI V keycode {ANSI_V_KEYCODE}"
                );
                Key::Other(u32::from(ANSI_V_KEYCODE))
            }
        }
    }
}

/// Wrapper for Enigo to store in Tauri's managed state.
/// Enigo is wrapped in a Mutex since it requires mutable access.
pub struct EnigoState(pub Mutex<Enigo>);

impl EnigoState {
    pub fn new() -> Result<Self, String> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| format!("Failed to initialize Enigo: {}", e))?;
        Ok(Self(Mutex::new(enigo)))
    }
}

/// Get the current mouse cursor position using the managed Enigo instance.
/// Returns None if the state is not available or if getting the location fails.
pub fn get_cursor_position(app_handle: &AppHandle) -> Option<(i32, i32)> {
    let enigo_state = app_handle.try_state::<EnigoState>()?;
    let enigo = enigo_state.0.lock().ok()?;
    enigo.location().ok()
}

/// The one operation the chord helpers need from enigo.
///
/// It exists so the release-on-failure guarantee below can be tested at all:
/// `Enigo::new` needs a display server, and the behaviour worth pinning is what
/// happens when an injection *fails*, which a real one will not do on demand.
pub(crate) trait KeyInjector {
    fn inject(&mut self, key: Key, direction: enigo::Direction) -> Result<(), String>;
}

impl KeyInjector for Enigo {
    fn inject(&mut self, key: Key, direction: enigo::Direction) -> Result<(), String> {
        self.key(key, direction).map_err(|e| e.to_string())
    }
}

/// Hold `modifiers` down, run `body`, then release them whatever `body` did.
///
/// Every chord below used to press a modifier and then `?` out of the next
/// step, which returns with the modifier still down. Nothing further releases
/// it: enigo's own held-key cleanup runs on `Drop`, and [`EnigoState`] is a
/// Tauri-managed singleton that lives as long as the process. The operating
/// system is then left believing that key is held, and the whole desktop
/// behaves as if the user were leaning on Ctrl until they press and release it
/// themselves.
///
/// The injection genuinely can fail mid-chord: `SendInput` is refused when the
/// foreground window runs at a higher integrity level than Handy (an elevated
/// terminal, Task Manager, a UAC prompt), and enigo reports that as an error on
/// whichever step hit it.
///
/// Release order is the reverse of press order, and only keys actually pressed
/// are released, so a failure part-way through a multi-modifier chord does not
/// send an unbalanced release for a key that was never accepted.
pub(crate) fn with_modifiers_held<E: KeyInjector>(
    enigo: &mut E,
    modifiers: &[(Key, &str)],
    hold_ms: u64,
    body: impl FnOnce(&mut E) -> Result<(), String>,
) -> Result<(), String> {
    let mut pressed: Vec<(Key, &str)> = Vec::with_capacity(modifiers.len());
    let mut outcome = Ok(());

    for &(key, name) in modifiers {
        match enigo.inject(key, enigo::Direction::Press) {
            Ok(()) => pressed.push((key, name)),
            Err(e) => {
                outcome = Err(format!("Failed to press {name} key: {e}"));
                break;
            }
        }
    }

    if outcome.is_ok() {
        outcome = body(enigo);
        if outcome.is_ok() {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
    }

    for (key, name) in pressed.into_iter().rev() {
        if let Err(e) = enigo.inject(key, enigo::Direction::Release) {
            // The one failure this function cannot repair. Retrying is pointless
            // against the cause: the foreground window that refused the
            // injection is still in the foreground a millisecond later. Say so
            // loudly instead, because the user is about to meet a desktop that
            // thinks the key is down and will have no idea why.
            log::error!(
                "Failed to release {name} after the paste chord: {e}. \
                 The system may treat {name} as held until it is pressed and released again."
            );
            if outcome.is_ok() {
                outcome = Err(format!("Failed to release {name} key: {e}"));
            }
        }
    }

    outcome
}

/// Sends a Ctrl+V or Cmd+V paste command using platform-specific virtual key codes.
/// This ensures the paste works regardless of keyboard layout (e.g., Russian, AZERTY, DVORAK).
/// Note: On Wayland, this may not work - callers should check for Wayland and use alternative methods.
///
/// `hold_ms` is how long the modifier stays held after the V click before being
/// released. Most applications read the modifier from the V event's flags and
/// need no hold at all, but applications that poll global keyboard state when
/// handling the key need the modifier to still be down — the hold insures
/// against those. Callers that can detect a failed chord (e.g. the
/// receipt-sequenced paste path) may use a much shorter hold.
pub fn send_paste_ctrl_v(enigo: &mut Enigo, hold_ms: u64) -> Result<(), String> {
    // Platform-specific key definitions
    #[cfg(target_os = "macos")]
    let (modifier_key, v_key_code) = (Key::Meta, macos::command_v_key());
    #[cfg(target_os = "windows")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Other(0x56)); // VK_V
    #[cfg(target_os = "linux")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Unicode('v'));

    with_modifiers_held(enigo, &[(modifier_key, "modifier")], hold_ms, |enigo| {
        enigo
            .inject(v_key_code, enigo::Direction::Click)
            .map_err(|e| format!("Failed to click V key: {}", e))
    })
}

/// Sends a Ctrl+Shift+V paste command.
/// This is commonly used in terminal applications on Linux to paste without formatting.
/// Note: On Wayland, this may not work - callers should check for Wayland and use alternative methods.
pub fn send_paste_ctrl_shift_v(enigo: &mut Enigo, hold_ms: u64) -> Result<(), String> {
    // Platform-specific key definitions
    #[cfg(target_os = "macos")]
    let (modifier_key, v_key_code) = (Key::Meta, macos::command_v_key());
    #[cfg(target_os = "windows")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Other(0x56)); // VK_V
    #[cfg(target_os = "linux")]
    let (modifier_key, v_key_code) = (Key::Control, Key::Unicode('v'));

    with_modifiers_held(
        enigo,
        &[(modifier_key, "modifier"), (Key::Shift, "Shift")],
        hold_ms,
        |enigo| {
            enigo
                .inject(v_key_code, enigo::Direction::Click)
                .map_err(|e| format!("Failed to click V key: {}", e))
        },
    )
}

/// Sends a Shift+Insert paste command (Windows and Linux only).
/// This is more universal for terminal applications and legacy software.
/// Note: On Wayland, this may not work - callers should check for Wayland and use alternative methods.
pub fn send_paste_shift_insert(enigo: &mut Enigo, hold_ms: u64) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let insert_key_code = Key::Other(0x2D); // VK_INSERT
    #[cfg(not(target_os = "windows"))]
    let insert_key_code = Key::Other(0x76); // XK_Insert (keycode 118 / 0x76, also used as fallback)

    with_modifiers_held(enigo, &[(Key::Shift, "Shift")], hold_ms, |enigo| {
        enigo
            .inject(insert_key_code, enigo::Direction::Click)
            .map_err(|e| format!("Failed to click Insert key: {}", e))
    })
}

/// Pastes text directly using the enigo text method.
/// This tries to use system input methods if possible, otherwise simulates keystrokes one by one.
pub fn paste_text_direct(enigo: &mut Enigo, text: &str) -> Result<(), String> {
    enigo
        .text(text)
        .map_err(|e| format!("Failed to send text directly: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use enigo::Direction;

    /// Records every injection and fails the nth one, which is the case a real
    /// `Enigo` will not reproduce on request.
    struct FakeInjector {
        log: Vec<(Key, Direction)>,
        fail_on: Option<usize>,
        calls: usize,
    }

    impl FakeInjector {
        fn new(fail_on: Option<usize>) -> Self {
            Self {
                log: Vec::new(),
                fail_on,
                calls: 0,
            }
        }

        /// Net presses per key. Anything left above zero is a key the operating
        /// system still believes is held.
        fn still_held(&self) -> Vec<Key> {
            let mut held: Vec<Key> = Vec::new();
            for (key, direction) in &self.log {
                match direction {
                    Direction::Press => held.push(*key),
                    Direction::Release => held.retain(|k| k != key),
                    Direction::Click => {}
                }
            }
            held
        }
    }

    impl KeyInjector for FakeInjector {
        fn inject(&mut self, key: Key, direction: Direction) -> Result<(), String> {
            self.calls += 1;
            if self.fail_on == Some(self.calls) {
                // The shape enigo reports when SendInput is refused, which is
                // what a higher-integrity foreground window causes on Windows.
                return Err("not all input events were sent".to_string());
            }
            self.log.push((key, direction));
            Ok(())
        }
    }

    #[test]
    fn a_failed_keystroke_still_releases_the_modifier() {
        // Call 1 presses Ctrl, call 2 is the V click. Failing the click used to
        // return with Ctrl down and nothing anywhere released it.
        let mut fake = FakeInjector::new(Some(2));

        let result = with_modifiers_held(&mut fake, &[(Key::Control, "Control")], 0, |enigo| {
            enigo.inject(Key::Other(0x56), Direction::Click)
        });

        assert!(result.is_err(), "the failure must still be reported");
        assert!(
            fake.still_held().is_empty(),
            "left held: {:?}",
            fake.still_held()
        );
    }

    #[test]
    fn a_failed_release_is_reported_rather_than_swallowed() {
        // Press, click, then fail the release itself. Nothing can repair this
        // one, so the contract is only that it does not pass as success.
        let mut fake = FakeInjector::new(Some(3));

        let result = with_modifiers_held(&mut fake, &[(Key::Control, "Control")], 0, |enigo| {
            enigo.inject(Key::Other(0x56), Direction::Click)
        });

        assert!(result.is_err());
    }

    #[test]
    fn a_modifier_that_was_never_pressed_is_never_released() {
        // Ctrl presses, Shift fails. Releasing Shift here would send an
        // unbalanced key-up for a key the system never saw go down.
        let mut fake = FakeInjector::new(Some(2));

        let result = with_modifiers_held(
            &mut fake,
            &[(Key::Control, "Control"), (Key::Shift, "Shift")],
            0,
            |_| Ok(()),
        );

        assert!(result.is_err());
        assert!(fake.still_held().is_empty());
        assert!(
            !fake
                .log
                .iter()
                .any(|(key, dir)| *key == Key::Shift && *dir == Direction::Release),
            "released a key that was never pressed: {:?}",
            fake.log
        );
    }

    #[test]
    fn modifiers_are_released_in_reverse_order() {
        let mut fake = FakeInjector::new(None);

        with_modifiers_held(
            &mut fake,
            &[(Key::Control, "Control"), (Key::Shift, "Shift")],
            0,
            |enigo| enigo.inject(Key::Other(0x56), Direction::Click),
        )
        .unwrap();

        assert_eq!(
            fake.log,
            vec![
                (Key::Control, Direction::Press),
                (Key::Shift, Direction::Press),
                (Key::Other(0x56), Direction::Click),
                (Key::Shift, Direction::Release),
                (Key::Control, Direction::Release),
            ]
        );
    }

    #[test]
    fn the_happy_path_leaves_nothing_held() {
        let mut fake = FakeInjector::new(None);
        with_modifiers_held(&mut fake, &[(Key::Control, "Control")], 0, |enigo| {
            enigo.inject(Key::Other(0x56), Direction::Click)
        })
        .unwrap();
        assert!(fake.still_held().is_empty());
    }
}
