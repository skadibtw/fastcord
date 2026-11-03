//! Per-user Classes registration. Never writes HKCR, machine-wide keys or UserChoice.
#![allow(unsafe_code)] // Registry FFI is confined to owned RAII handles below.
use super::{Error, executable};
use ::windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};
use ::windows::Win32::System::Registry::*;
use ::windows::core::{HSTRING, PCWSTR};
use std::path::Path;

const OWNER: &str = "FastcordLinkOwner";
const SCHEME: &str = "fastcord";
const USER_KEY: &str = r"Software\Classes\fastcord";
const CHOICE: &str =
    r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\fastcord\UserChoice";

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: this handle was returned by a successful registry open/create.
        let _ = unsafe { RegCloseKey(self.0) };
    }
}
fn open(root: HKEY, path: &str) -> Result<Option<Key>, Error> {
    open_access(root, path, KEY_READ)
}
fn open_access(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> Result<Option<Key>, Error> {
    let mut key = HKEY::default();
    // SAFETY: valid terminated input and out-parameter; production callers request read-only access.
    let status = unsafe { RegOpenKeyExW(root, &HSTRING::from(path), None, access, &mut key) };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
        return Ok(None);
    }
    status.ok().map_err(|_| Error::Failed)?;
    Ok(Some(Key(key)))
}
fn value(key: &Key, name: &str) -> Result<Option<String>, Error> {
    let mut bytes = [0u8; 8192];
    let mut size = bytes.len() as u32;
    let mut kind = REG_VALUE_TYPE::default();
    // SAFETY: fixed output buffer and its exact capacity; native API bounds writes.
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            &HSTRING::from(name),
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status.ok().map_err(|_| Error::Failed)?;
    if kind != REG_SZ || size < 2 || !size.is_multiple_of(2) || size as usize > bytes.len() {
        return Err(Error::Conflict);
    }
    let mut units: Vec<_> = bytes[..size as usize]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    if units.pop() != Some(0) || units.contains(&0) {
        return Err(Error::Conflict);
    }
    String::from_utf16(&units)
        .map(Some)
        .map_err(|_| Error::Conflict)
}
fn write(key: &Key, name: &str, value: &str) -> Result<(), Error> {
    let bytes: Vec<_> = value
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    // SAFETY: valid live handle, terminated value name and UTF-16 byte buffer.
    unsafe { RegSetValueExW(key.0, &HSTRING::from(name), None, REG_SZ, Some(&bytes)) }
        .ok()
        .map_err(|_| Error::Failed)
}
fn create_new(root: HKEY, path: &str) -> Result<Key, Error> {
    let mut key = HKEY::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
    // SAFETY: valid terminated input and output pointers. Only per-user/test root
    // callers are allowed; no delete, security-change or machine-wide rights.
    unsafe {
        RegCreateKeyExW(
            root,
            &HSTRING::from(path),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_CREATE_SUB_KEY,
            None,
            &mut key,
            Some(&mut disposition),
        )
    }
    .ok()
    .map_err(|_| Error::Failed)?;
    let key = Key(key);
    if disposition != REG_CREATED_NEW_KEY {
        return Err(Error::Conflict);
    }
    Ok(key)
}
fn shape(key: &Key, subkeys: u32, values: u32) -> Result<(), Error> {
    let mut actual_subkeys = 0;
    let mut actual_values = 0;
    // SAFETY: live read-only key and valid count output pointers.
    unsafe {
        RegQueryInfoKeyW(
            key.0,
            None,
            None,
            None,
            Some(&mut actual_subkeys),
            None,
            None,
            Some(&mut actual_values),
            None,
            None,
            None,
            None,
        )
    }
    .ok()
    .map_err(|_| Error::Failed)?;
    if actual_subkeys != subkeys || actual_values != values {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn owned(root: HKEY, path: &str, command: &str) -> Result<bool, Error> {
    let Some(key) = open(root, path)? else {
        return Ok(false);
    };
    // Accept only the exact key structure this module creates. Shell default
    // verbs, DDE, CurVer, DelegateExecute and inherited extra keys must not
    // silently redirect the apparently-owned command.
    shape(&key, 1, 3)?;
    let shell = open(key.0, "shell")?.ok_or(Error::Conflict)?;
    shape(&shell, 1, 0)?;
    let open_key = open(shell.0, "open")?.ok_or(Error::Conflict)?;
    shape(&open_key, 1, 0)?;
    let cmd = open(open_key.0, "command")?.ok_or(Error::Conflict)?;
    shape(&cmd, 0, 1)?;
    if value(&key, OWNER)?.as_deref() != Some("1")
        || value(&key, "URL Protocol")?.as_deref() != Some("")
        || value(&cmd, "")?.as_deref() != Some(command)
        || value(&cmd, "DelegateExecute")?.is_some()
    {
        return Err(Error::Conflict);
    }
    Ok(true)
}
fn command(exe: &Path) -> Result<String, Error> {
    Ok(format!("\"{}\" --open-url \"%1\"", executable(exe)?))
}
pub(super) fn register(exe: &Path) -> Result<(), Error> {
    register_at(
        exe,
        HKEY_CURRENT_USER,
        USER_KEY,
        HKEY_CLASSES_ROOT,
        SCHEME,
        HKEY_CURRENT_USER,
        CHOICE,
    )?;
    // SAFETY: documented association-change event has no item pointers. This
    // only invalidates Shell caches after the explicit production action;
    // private-namespace tests call register_at and never notify the host Shell.
    unsafe {
        ::windows::Win32::UI::Shell::SHChangeNotify(
            ::windows::Win32::UI::Shell::SHCNE_ASSOCCHANGED,
            ::windows::Win32::UI::Shell::SHCNF_IDLIST,
            None,
            None,
        );
    }
    Ok(())
}
fn register_at(
    exe: &Path,
    user: HKEY,
    user_path: &str,
    merged: HKEY,
    merged_path: &str,
    choices: HKEY,
    choice_path: &str,
) -> Result<(), Error> {
    let command = command(exe)?;
    let check_choice = || -> Result<(), Error> {
        if let Some(choice) = open(choices, choice_path)?
            && value(&choice, "ProgId")?.as_deref() != Some(SCHEME)
        {
            return Err(Error::Conflict);
        }
        Ok(())
    };
    check_choice()?;
    // HKCR is authoritative for effective machine/user Classes ownership.
    // Do not shadow an existing foreign machine-wide registration.
    let effective = owned(merged, merged_path, &command)?;
    let local = owned(user, user_path, &command)?;
    if effective || local {
        return if effective && local {
            Ok(())
        } else {
            Err(Error::Conflict)
        };
    }
    check_choice()?;
    // Atomic create disposition prevents overwriting a newly-created user key.
    let key = create_new(user, user_path)?;
    write(&key, OWNER, "1")?;
    write(&key, "", "URL:fastcord links")?;
    write(&key, "URL Protocol", "")?;
    let cmd = create_new(key.0, r"shell\open\command")?;
    write(&cmd, "", &command)?;
    check_choice()?;
    if !owned(merged, merged_path, &command)? {
        return Err(Error::Failed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temporary_registry_namespace_keeps_foreign_handlers_and_quotes_current_exe() {
        let base = format!(r"Software\fastcord-test-links-{}", std::process::id());
        let root = create_new(HKEY_CURRENT_USER, &base).unwrap();
        let exe = Path::new(r"C:\Program Files\Fast Cord\fastcord.exe");
        let result = std::panic::catch_unwind(|| {
            assert_eq!(
                command(exe).unwrap(),
                r#""C:\Program Files\Fast Cord\fastcord.exe" --open-url "%1""#
            );
            // The isolated merged view points to the same test subtree, never HKCR.
            register_at(exe, root.0, "own", root.0, "own", root.0, "choice").unwrap();
            register_at(exe, root.0, "own", root.0, "own", root.0, "choice").unwrap();
            assert_eq!(
                register_at(
                    Path::new(r"C:\Other\fastcord.exe"),
                    root.0,
                    "own",
                    root.0,
                    "own",
                    root.0,
                    "choice"
                ),
                Err(Error::Conflict)
            );
            let original_command = open(root.0, r"own\shell\open\command").unwrap().unwrap();
            assert_eq!(
                value(&original_command, "").unwrap(),
                Some(command(exe).unwrap())
            );
            let shell = open_access(root.0, r"own\shell", KEY_QUERY_VALUE | KEY_SET_VALUE)
                .unwrap()
                .unwrap();
            // Extra default-verb state must not be reported as our handler.
            // Reuse a second isolated registration for all remaining cases.
            write(&shell, "", "foreign-verb").unwrap();
            assert_eq!(
                register_at(exe, root.0, "own", root.0, "own", root.0, "choice"),
                Err(Error::Conflict)
            );
            register_at(exe, root.0, "clean", root.0, "clean", root.0, "choice").unwrap();
            let foreign = create_new(root.0, "foreign").unwrap();
            write(&foreign, "", "keep").unwrap();
            assert_eq!(
                register_at(exe, root.0, "new", root.0, "foreign", root.0, "choice"),
                Err(Error::Conflict)
            );
            assert!(open(root.0, "new").unwrap().is_none());
            assert_eq!(value(&foreign, "").unwrap().as_deref(), Some("keep"));
            let choice = create_new(root.0, "choice").unwrap();
            write(&choice, "ProgId", "other").unwrap();
            assert_eq!(
                register_at(exe, root.0, "clean", root.0, "clean", root.0, "choice"),
                Err(Error::Conflict)
            );
            assert_eq!(value(&choice, "ProgId").unwrap().as_deref(), Some("other"));
        });
        drop(root);
        // SAFETY: delete only this test's uniquely-created private namespace.
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(&base)) }
            .ok()
            .unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
