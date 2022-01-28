// SPDX-License-Identifier: GPL-3.0-only
use anyhow::{anyhow, Result};
use futures::{channel::mpsc::Receiver, SinkExt};
use glib::translate::ToGlibPtr;
use gtk4::glib::object::Cast;
use gtk4::prelude::ObjectExt;
use gtk4::{glib, CssProvider, Orientation};
use libloading::{Library, Symbol};
use log::debug;
use notify::{Event, INotifyWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
// A plugin which allows you to add extra functionality to the cosmic dock/panel.
use std::ffi::c_void;
use thin_trait_object::*;

#[thin_trait_object(drop_abi = "C")]
pub trait Plugin {
    extern "C" fn _applet(&mut self) -> *mut gtk4_sys::GtkBox {
        self.applet().to_glib_full()
    }
    extern "C" fn _css_provider(&mut self) -> *mut gtk4_sys::GtkCssProvider {
        self.css_provider().to_glib_full()
    }
    extern "C" fn _on_plugin_load(&mut self) {
        self.on_plugin_load();
    }
    extern "C" fn _on_plugin_unload(&mut self) {
        self.on_plugin_unload();
    }

    /// Get the applet
    fn applet(&mut self) -> gtk4::Box;
    /// get the css provider
    fn css_provider(&mut self) -> CssProvider {
        CssProvider::new()
    }
    /// A callback fired immediately after the plugin is loaded. Usually used
    /// for initialization.
    fn on_plugin_load(&mut self) {
        gtk4::init().unwrap();
    }
    /// A callback fired immediately before the plugin is unloaded. Use this if
    /// you need to do any cleanup.
    fn on_plugin_unload(&mut self) {}
}

#[macro_export]
macro_rules! declare_plugin {
    ($plugin_type:ty) => {
        use gtk4::glib::translate::ToGlibPtr;
        #[no_mangle]
        pub extern "C" fn _plugin_create() -> *mut std::ffi::c_void {
            // make sure the constructor is the correct type.
            let constructor: fn() -> $plugin_type = <$plugin_type>::default;

            let object = constructor();
            let boxed_plugin: BoxedPlugin = BoxedPlugin::new(object);
            boxed_plugin.into_raw() as *mut std::ffi::c_void
        }
    };
}

pub(crate) struct PluginLibrary<'a> {
    pub(crate) name: String,
    pub(crate) lib_path: OsString,
    pub(crate) plugin: BoxedPlugin<'a>,
    pub(crate) css_provider: CssProvider,
    pub(crate) applet: gtk4::Box,
    pub(crate) loaded_library: Library,
}

/// library should only be unloaded and dropped after no more references tro its applet are being used.
impl<'a> Drop for PluginLibrary<'a> {
    fn drop(&mut self) {
        let PluginLibrary {
            name,
            lib_path: filename,
            plugin,
            css_provider,
            applet,
            loaded_library,
        } = self;
        plugin.on_plugin_unload();
        drop(applet);
        drop(name);
        drop(filename);
        drop(css_provider);
        drop(plugin);
        // XXX must be dropped last
        drop(loaded_library);
    }
}

#[derive(Default)]
pub struct PluginManager<'a> {
    plugins: Vec<PluginLibrary<'a>>,
    watcher: Option<INotifyWatcher>,
    watching: Vec<(String, PathBuf)>,
}

impl<'a> PluginManager<'a> {
    pub fn new() -> (PluginManager<'a>, Option<Receiver<notify::Result<Event>>>) {
        // setup library watcher
        match async_watcher() {
            Ok((watcher, rx)) => (
                PluginManager {
                    plugins: Vec::new(),
                    watcher: Some(watcher),
                    ..Default::default()
                },
                Some(rx),
            ),
            Err(e) => {
                eprintln!("{}", e);
                (Default::default(), None)
            }
        }
    }

    /// library should only be unloaded and dropped after no more references to its applet are being used.
    pub unsafe fn unload_plugin<P: AsRef<OsStr>>(&mut self, lib_path: P) {
        if let Some(i) = self.plugins.iter().enumerate().find_map(|(i, p)| {
            if p.lib_path == lib_path.as_ref() {
                Some(i)
            } else {
                None
            }
        }) {
            self.plugins.remove(i);
        }
    }

    pub unsafe fn load_plugin<P: AsRef<OsStr> + Into<String> + Clone>(
        &mut self,
        name: P,
    ) -> Result<(&gtk4::Box, &CssProvider)> {
        type PluginCreate<'a> = unsafe fn() -> *mut c_void;

        let lib_path = get_ld_path(name.as_ref()).ok_or(anyhow!("library could not be found."))?;
        let lib = Library::new(&lib_path)?;
        self.watch_library(&lib_path.parent().unwrap())?;
        // We need to keep the library around otherwise our plugin's vtable will
        // point to garbage.

        let constructor: Symbol<PluginCreate> = lib.get(b"_plugin_create")?;
        let boxed_raw = constructor();

        let mut plugin = BoxedPlugin::from_raw(boxed_raw as *mut ());
        plugin.on_plugin_load();

        // XXX gtk needs to be initialized before loading applet and css provider
        // let get_applet: Symbol<GetApplet> = lib.get(b"_applet")?;
        let applet = plugin._applet();
        let applet: gtk4::Box = if !applet.is_null() {
            gtk4::glib::translate::from_glib_full::<_, gtk4::Box>(applet).unsafe_cast()
        } else {
            gtk4::Box::new(Orientation::Vertical, 0)
        };

        // get css provider
        let css_provider = plugin._css_provider();
        let css_provider: CssProvider = if !css_provider.is_null() {
            gtk4::glib::translate::from_glib_full(css_provider)
        } else {
            CssProvider::new()
        };

        self.plugins.push(PluginLibrary {
            name: name.clone().into(),
            lib_path: lib_path.clone().into(),
            plugin,
            css_provider,
            applet,
            loaded_library: lib,
        });
        self.watching.push((name.into(), lib_path));
        let PluginLibrary {
            applet,
            css_provider,
            ..
        } = self.plugins.last().unwrap();

        Ok((applet, css_provider))
    }

    /// Unload all plugins and loaded plugin libraries, making sure to fire
    /// their `on_plugin_unload()` methods so they can do any necessary cleanup.
    /// library should only be unloaded and dropped after no more references to its applet are being used.
    pub fn unload_all(&mut self) {
        debug!("Unloading plugins");
        for p in self.plugins.drain(..) {
            drop(p);
        }
        if let Some(watcher) = self.watcher.as_mut() {
            for (_, f) in self.watching.drain(..) {
                let _ = watcher.unwatch(f.as_ref());
            }
        }
    }

    pub fn paths(&self) -> Vec<OsString> {
        self.plugins.iter().map(|p| p.lib_path.clone()).collect()
    }

    pub fn library_path_to_applet<T: AsRef<OsStr>>(&self, lib_filename: T) -> Option<&gtk4::Box> {
        self.plugins.iter().find_map(move |p| {
            if p.lib_path.as_os_str() == lib_filename.as_ref() {
                Some(&p.applet)
            } else {
                None
            }
        })
    }

    pub fn library_path_to_name<T: AsRef<OsStr>>(&self, lib_filename: T) -> Option<String> {
        self.watching.iter().find_map(move |(name, filename)| {
            if filename.as_os_str() == lib_filename.as_ref() {
                Some(name.clone())
            } else {
                None
            }
        })
    }

    fn watch_library<P: AsRef<Path>>(&mut self, path: P) -> notify::Result<()> {
        if let Some(watcher) = self.watcher.as_mut() {
            watcher.watch(&path.as_ref(), RecursiveMode::NonRecursive)?
        }
        Ok(())
    }
}

fn async_watcher() -> notify::Result<(INotifyWatcher, Receiver<notify::Result<Event>>)> {
    use futures::channel::mpsc::channel;
    let (mut tx, rx) = channel(100);

    let watcher = INotifyWatcher::new(move |res| {
        futures::executor::block_on(async {
            tx.send(res).await.unwrap();
        })
    })?;

    Ok((watcher, rx))
}

pub fn get_path_to_xdg_data<T: AsRef<Path>>(name: T) -> Option<PathBuf> {
    let mut data_dirs = vec![gtk4::glib::user_data_dir()];
    data_dirs.append(&mut gtk4::glib::system_data_dirs());
    for mut p in data_dirs {
        p.push(&name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

pub fn get_ld_path<T: AsRef<Path>>(lib_name: T) -> Option<PathBuf> {
    let filename = libloading::library_filename(lib_name.as_ref());
    let ld_library_dirs: Vec<PathBuf> = std::env::var("LD_LIBRARY_PATH")
        .map(|dirs| dirs.split(":").map(|s| PathBuf::from(s)).collect())
        .unwrap_or_default();
    for mut path in ld_library_dirs {
        path.push(&filename);
        if path.exists() {
            return Some(path);
        }
    }

    // check output of ldconfig
    if let Some(Ok(re)) = &filename
        .to_str()
        .map(|s| Regex::new(format!(r"\s*{}\s.*=>\s(.+)\s", s).as_str()))
    {
        if let Ok(Ok(cap)) = Command::new("ldconfig")
            .arg("-p")
            .output()
            .map(|o| String::from_utf8(o.stdout))
            .map(|o| {
                re.captures_iter(&o?)
                    .next()
                    .map(|cap| cap[1].to_string())
                    .ok_or(anyhow!("no match"))
            })
        {
            return Some(cap.into());
        }
    }
    None
}
