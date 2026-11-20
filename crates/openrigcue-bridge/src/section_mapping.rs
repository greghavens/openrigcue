//! A read-only named mapping: create/open, then map the exact
//! requested read-only view. Kept separate so failure cleanup is testable with
//! owned fake handles, without creating a Windows mapping.

use std::io;

pub(crate) trait ReadOnlyApi {
    type Section;
    type View;
    fn create_read_only(&mut self, name: &str, size: usize) -> io::Result<Self::Section>;
    fn map_read(&mut self, section: &Self::Section, size: usize) -> io::Result<Self::View>;
    fn copy(&mut self, view: &Self::View) -> Vec<u8>;
}

pub(crate) struct ReadOnlyMapping<S, V> {
    // Unmap the view before closing its section handle.
    pub(crate) view: V,
    pub(crate) section: S,
}

pub(crate) fn open<A: ReadOnlyApi>(
    api: &mut A,
    name: &str,
    size: usize,
) -> io::Result<ReadOnlyMapping<A::Section, A::View>> {
    let section = api.create_read_only(name, size)?;
    let view = api.map_read(&section, size)?;
    Ok(ReadOnlyMapping { view, section })
}

pub(crate) fn copy_once<A: ReadOnlyApi>(api: &mut A, name: &str, size: usize) -> io::Result<Vec<u8>> {
    let mapping = open(api, name, size)?;
    let bytes = api.copy(&mapping.view);
    drop(mapping);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Resource(&'static str, Arc<Mutex<Vec<String>>>);
    impl Drop for Resource {
        fn drop(&mut self) {
            self.1.lock().unwrap().push(self.0.into());
        }
    }
    struct Api {
        events: Arc<Mutex<Vec<String>>>,
        bytes: Option<Vec<u8>>,
        create_error: bool,
        view_error: bool,
    }
    impl ReadOnlyApi for Api {
        type Section = Resource;
        type View = (Resource, Vec<u8>);
        fn create_read_only(&mut self, name: &str, size: usize) -> io::Result<Resource> {
            self.events.lock().unwrap().push(format!("CreateFileMapping PAGE_READONLY {name} {size}"));
            if self.create_error {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            Ok(Resource("CloseHandle", self.events.clone()))
        }
        fn map_read(&mut self, _: &Resource, size: usize) -> io::Result<Self::View> {
            self.events.lock().unwrap().push(format!("MapViewOfFile FILE_MAP_READ {size}"));
            if self.view_error {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let bytes = self.bytes.clone().unwrap_or_else(|| vec![0; size]);
            if bytes.len() < size {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            Ok((Resource("UnmapViewOfFile", self.events.clone()), bytes[..size].to_vec()))
        }
        fn copy(&mut self, view: &Self::View) -> Vec<u8> {
            self.events.lock().unwrap().push("copy".into());
            view.1.clone()
        }
    }
    fn api() -> Api {
        Api { events: Arc::default(), bytes: None, create_error: false, view_error: false }
    }
    #[test]
    fn missing_read_only_section_is_zero_filled_and_copied_before_release() {
        let mut api = api();
        assert_eq!(copy_once(&mut api, "$pcars2$", 4).unwrap(), [0; 4]);
        assert_eq!(
            *api.events.lock().unwrap(),
            [
                "CreateFileMapping PAGE_READONLY $pcars2$ 4",
                "MapViewOfFile FILE_MAP_READ 4",
                "copy",
                "UnmapViewOfFile",
                "CloseHandle",
            ]
        );
    }
    #[test]
    fn existing_read_only_view_copies_requested_prefix_without_resizing() {
        let mut api = api();
        api.bytes = Some(vec![1, 2, 3, 4]);
        let mapping = open(&mut api, "$R3E", 2).unwrap();
        assert_eq!(mapping.section.0, "CloseHandle");
        assert_eq!(api.copy(&mapping.view), [1, 2]);
        assert_eq!(api.events.lock().unwrap().len(), 3, "the held mapping remains live");
        drop(mapping);
        assert_eq!(&api.events.lock().unwrap()[3..], ["UnmapViewOfFile", "CloseHandle"]);
    }
    #[test]
    fn create_failure_propagates_without_mapping_or_closing_an_invalid_handle() {
        let mut api = api();
        api.create_error = true;
        assert!(matches!(open(&mut api, "x", 4), Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
        assert_eq!(api.events.lock().unwrap().len(), 1);
    }
    #[test]
    fn view_failure_closes_the_acquired_handle_before_returning_error() {
        for small in [false, true] {
            let mut api = api();
            api.view_error = !small;
            api.bytes = Some(vec![1]);
            assert!(matches!(copy_once(&mut api, "x", 4), Err(error) if error.kind() == io::ErrorKind::InvalidInput));
            assert_eq!(&api.events.lock().unwrap()[2..], ["CloseHandle"]);
        }
    }
}
