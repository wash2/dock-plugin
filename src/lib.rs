// SPDX-License-Identifier: GPL-3.0-only
use anyhow::Result;
use gtk4::glib::object::Cast;
use gtk4::Orientation;
use gtk4_sys::GtkWidget;
use libloading::{Library, Symbol};
use log::{debug, trace};
use std::any::Any;
use std::ffi::OsStr;

/// A plugin which allows you to add extra functionality to the cosmic dock/panel.
pub trait Plugin: Any + Send + Sync {
    /// Get a name describing the `Plugin`.
    fn name(&self) -> &'static str;
    /// Get the applet
    fn applet(&self) -> gtk4::Box;
    /// A callback fired immediately after the plugin is loaded. Usually used
    /// for initialization.
    fn on_plugin_load(&self) {
        gtk4::init().unwrap();
    }
    /// A callback fired immediately before the plugin is unloaded. Use this if
    /// you need to do any cleanup.
    fn on_plugin_unload(&self) {}
}

/// Declare a plugin type and its constructor.
///
/// # Notes
///
/// This works by automatically generating an `extern "C"` function with a
/// pre-defined signature and symbol name. Therefore you will only be able to
/// declare one plugin per library.
#[macro_export]
macro_rules! declare_plugin {
    ($plugin_type:ty, $constructor:path, $applet:path) => {
        use gtk4::glib::translate::ToGlibPtr;
        #[no_mangle]
        pub extern "C" fn _plugin_create() -> *mut dyn $crate::Plugin {
            // make sure the constructor is the correct type.
            let constructor: fn() -> $plugin_type = $constructor;

            let object = constructor();
            let boxed: std::boxed::Box<dyn $crate::Plugin> = std::boxed::Box::new(object);
            std::boxed::Box::into_raw(boxed)
        }
        #[no_mangle]
        pub extern "C" fn _applet(self_: *const $plugin_type) -> *const gtk4_sys::GtkWidget {
            let applet: fn(&$plugin_type) -> gtk4::Box = $applet;
            let self_ = unsafe { self_.as_ref().unwrap() };
            let widget = applet(self_);
            let boxed: std::boxed::Box<gtk4::Box> = std::boxed::Box::new(widget);
            unsafe {
                let b: gtk4::glib::translate::Stash<
                    'static,
                    *const gtk4_sys::GtkWidget,
                    gtk4::Box,
                > = std::boxed::Box::into_raw(boxed)
                    .as_ref()
                    .unwrap()
                    .to_glib_none();
                b.0
            }
        }
    };
}

pub(crate) struct PluginLibrary {
    pub(crate) plugin: Box<dyn Plugin>,
    pub(crate) applet: gtk4::Box,
    pub(crate) loaded_library: Library,
}

pub struct PluginManager {
    plugins: Vec<PluginLibrary>,
}

impl PluginManager {
    pub fn new() -> PluginManager {
        PluginManager {
            plugins: Vec::new(),
        }
    }

    pub unsafe fn load_plugin<P: AsRef<OsStr>>(&mut self, filename: P) -> Result<()> {
        type PluginCreate = unsafe fn() -> *mut dyn Plugin;
        type GetApplet = unsafe fn() -> *const GtkWidget;

        let lib = Library::new(filename.as_ref())?;

        // We need to keep the library around otherwise our plugin's vtable will
        // point to garbage.

        let constructor: Symbol<PluginCreate> = lib.get(b"_plugin_create")?;
        let boxed_raw = constructor();

        let plugin = Box::from_raw(boxed_raw);
        debug!("Loaded plugin: {}", plugin.name());
        plugin.on_plugin_load();

        // gtk needs to be initialized before loading applet
        let get_applet: Symbol<GetApplet> = lib.get(b"_applet")?;
        let applet = get_applet();
        let applet: gtk4::Box = if !applet.is_null() {
            gtk4::glib::translate::from_glib_none::<_, gtk4::Widget>(applet).unsafe_cast()
        } else {
            gtk4::Box::new(Orientation::Vertical, 0)
        };

        self.plugins.push(PluginLibrary {
            plugin,
            applet,
            loaded_library: lib,
        });

        Ok(())
    }

    /// Unload all plugins and loaded plugin libraries, making sure to fire
    /// their `on_plugin_unload()` methods so they can do any necessary cleanup.
    pub fn unload(&mut self) {
        debug!("Unloading plugins");

        for PluginLibrary {
            plugin,
            applet,
            loaded_library,
        } in self.plugins.drain(..)
        {
            trace!("Firing on_plugin_unload for {:?}", plugin.name());
            plugin.on_plugin_unload();
            drop(applet);
            drop(plugin);
            drop(loaded_library);
        }
    }

    pub fn applets(&self) -> Vec<&gtk4::Box> {
        self.plugins.iter().map(|p| &p.applet).collect()
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        if !self.plugins.is_empty() {
            self.unload();
        }
    }
}
