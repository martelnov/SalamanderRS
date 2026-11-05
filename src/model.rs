use std::cell::Cell;

pub use salamander_core::model::*;

mod entry_object {
    use super::{Cell, FileEntry};
    use gtk::glib;
    use gtk::glib::object::ObjectExt;
    use gtk::subclass::prelude::*;

    mod imp {
        use super::*;

        #[derive(glib::Properties, Default)]
        #[properties(wrapper_type = super::EntryObject)]
        pub struct EntryObject {
            pub entry: std::cell::OnceCell<FileEntry>,
            #[property(get, set)]
            pub marked: Cell<bool>,
            /// Computed recursive size of a directory; -1 while unknown.
            #[property(get, set)]
            pub dir_size: Cell<i64>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for EntryObject {
            const NAME: &'static str = "SalamanderRsEntryObject";
            type Type = super::EntryObject;
        }

        #[glib::derived_properties]
        impl ObjectImpl for EntryObject {}
    }

    glib::wrapper! {
        pub struct EntryObject(ObjectSubclass<imp::EntryObject>);
    }

    impl EntryObject {
        pub fn new(entry: FileEntry) -> Self {
            let object: Self = glib::Object::builder().property("dir-size", -1i64).build();
            object
                .imp()
                .entry
                .set(entry)
                .expect("entry initialized once");
            object
        }

        pub fn with_entry<R>(&self, f: impl FnOnce(&FileEntry) -> R) -> R {
            f(self.imp().entry.get().expect("entry initialized"))
        }

        pub fn is_marked(&self) -> bool {
            self.imp().marked.get()
        }

        pub fn known_dir_size(&self) -> Option<u64> {
            u64::try_from(self.imp().dir_size.get()).ok()
        }

        /// Size used for sorting and totals: file size, or the computed directory size.
        pub fn effective_size(&self) -> u64 {
            self.with_entry(|entry| match entry.kind {
                super::EntryKind::File => entry.size,
                super::EntryKind::Directory => self.known_dir_size().unwrap_or(0),
                _ => 0,
            })
        }

        pub fn display_size(&self) -> String {
            match self.known_dir_size() {
                Some(size) if self.with_entry(|entry| entry.kind == super::EntryKind::Directory) => {
                    super::format_bytes(size)
                }
                _ => self.with_entry(super::format_size),
            }
        }
    }
}

pub use entry_object::EntryObject;
