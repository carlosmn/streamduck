//! Core and device configs
use std::collections::HashMap;
use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use image::{DynamicImage};
use serde::{Serialize, Deserialize};
use serde::de::DeserializeOwned;
use crate::core::RawButtonPanel;
use serde_json::Value;
use streamdeck::Kind;
use crate::ImageCollection;
use crate::images::{SDImage, SDSerializedImage};
use crate::util::{hash_image, hash_str};
use crate::thread::util::resize_for_streamdeck;

pub const DEFAULT_POOL_RATE: u32 = 1000;
pub const DEFAULT_RECONNECT_TIME: f32 = 1.0;
pub const DEFAULT_CONFIG_PATH: &'static str = "devices";
pub const DEFAULT_PLUGIN_PATH: &'static str = "plugins";
pub const DEFAULT_PLUGIN_SETTINGS_PATH: &'static str = "global.json";

/// Reference counted [DeviceConfig]
pub type UniqueDeviceConfig = Arc<RwLock<DeviceConfig>>;

/// Struct to keep daemon settings
#[derive(Serialize, Deserialize, Default)]
pub struct Config {
    /// Frequency of streamdeck event pooling
    pool_rate: Option<u32>,
    /// Frequency of checks for disconnected devices
    reconnect_rate: Option<f32>,
    /// Path to device configs
    device_config_path: Option<PathBuf>,
    /// Path to plugins
    plugin_path: Option<PathBuf>,
    /// Path to plugin settings json
    plugin_settings_path: Option<PathBuf>,

    #[serde(skip)]
    pub plugin_settings: RwLock<HashMap<String, Value>>,

    /// Currently loaded device configs
    #[serde(skip)]
    pub loaded_configs: RwLock<HashMap<String, UniqueDeviceConfig>>,

    /// Currently loaded image collections
    #[serde(skip)]
    pub loaded_images: RwLock<HashMap<String, ImageCollection>>
}

#[allow(dead_code)]
impl Config {
    /// Reads config and retrieves config struct
    pub fn get() -> Config {
        let config: Config = if let Ok(content) = fs::read_to_string("config.toml") {
            if let Ok(config) = toml::from_str(&content) {
                config
            } else {
                Default::default()
            }
        } else {
            Default::default()
        };

        config.load_plugin_settings();

        config
    }

    /// Pool rate, defaults to [DEFAULT_POOL_RATE] if not set
    pub fn pool_rate(&self) -> u32 {
        self.pool_rate.unwrap_or(DEFAULT_POOL_RATE)
    }

    /// Reconnect rate, defaults to [DEFAULT_RECONNECT_TIME] if not set
    pub fn reconnect_rate(&self) -> f32 {
        self.reconnect_rate.unwrap_or(DEFAULT_RECONNECT_TIME)
    }

    /// Device config path, defaults to [DEFAULT_CONFIG_PATH] if not set
    pub fn device_config_path(&self) -> PathBuf {
        self.device_config_path.clone().unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH))
    }

    /// Plugin folder path, defaults to [DEFAULT_PLUGIN_PATH] if not set
    pub fn plugin_path(&self) -> PathBuf {
        self.plugin_path.clone().unwrap_or(PathBuf::from(DEFAULT_PLUGIN_PATH))
    }

    /// Global config path, defaults to [DEFAULT_PLUGIN_SETTINGS_PATH] if not set
    pub fn plugin_settings_path(&self) -> PathBuf {
        self.plugin_settings_path.clone().unwrap_or(PathBuf::from(DEFAULT_PLUGIN_SETTINGS_PATH))
    }

    /// Loads plugin settings from file
    pub fn load_plugin_settings(&self) {
        if let Ok(settings) = fs::read_to_string(self.plugin_settings_path()) {
            let mut lock = self.plugin_settings.write().unwrap();

            match serde_json::from_str(&settings) {
                Ok(vals) => *lock = vals,
                Err(err) => log::error!("Failed to parse plugin settings: {:?}", err),
            }
        }
    }

    /// Retrieves plugin settings if it exists
    pub fn get_plugin_settings<T: PluginConfig + DeserializeOwned>(&self) -> Option<T> {
        let lock = self.plugin_settings.read().unwrap();
        Some(serde_json::from_value(lock.get(T::NAME)?.clone()).ok()?)
    }

    /// Sets plugin settings
    pub fn set_plugin_settings<T: PluginConfig + Serialize>(&self, value: T) {
        let mut lock = self.plugin_settings.write().unwrap();
        lock.insert(T::NAME.to_string(), serde_json::to_value(value).unwrap());
        drop(lock);

        self.write_plugin_settings();
    }

    /// Writes plugin settings to file
    pub fn write_plugin_settings(&self) {
        let lock = self.plugin_settings.read().unwrap();
        if let Err(err) = fs::write(self.plugin_settings_path(), serde_json::to_string(lock.deref()).unwrap()) {
            log::error!("Failed to write plugin settings: {:?}", err);
        }
    }

    /// Reloads device config for specified serial
    pub fn reload_device_config(&self, serial: &str) -> Result<(), ConfigError> {
        // Clearing image collection to make sure it's fresh for reload
        self.get_image_collection(serial).write().unwrap().clear();

        let mut devices = self.loaded_configs.write().unwrap();

        let mut path = self.device_config_path();
        path.push(format!("{}.json", serial));

        let content = fs::read_to_string(path)?;
        let device = serde_json::from_str::<DeviceConfig>(&content)?;


        if let Some(device_config) = devices.get(serial) {
            *device_config.write().unwrap() = device;
        } else {
            devices.insert(serial.to_string(), Arc::new(RwLock::new(device)));
        }

        self.update_collection(devices.get(serial).unwrap());

        Ok(())
    }

    /// Reloads all device configs
    pub fn reload_device_configs(&self) -> Result<(), ConfigError> {
        let mut devices = self.loaded_configs.write().unwrap();

        let dir = fs::read_dir(self.device_config_path())?;

        for item in dir {
            let item = item?;
            if item.path().is_file() {
                if let Some(extension) = item.path().extension() {
                    if extension == "json" {
                        let content = fs::read_to_string(item.path())?;

                        let device = serde_json::from_str::<DeviceConfig>(&content)?;
                        let serial = device.serial.to_string();

                        // Clearing image collection so it's fresh for reload
                        self.get_image_collection(&device.serial).write().unwrap().clear();
                        if let Some(device_config) = devices.get(&serial) {
                            *device_config.write().unwrap() = device;
                        } else {
                            devices.insert(serial.to_string(), Arc::new(RwLock::new(device)));
                        }

                        self.update_collection(devices.get(&serial).unwrap());
                    }
                }
            }
        }

        Ok(())
    }

    /// Saves device config for specified serial
    pub fn save_device_config(&self, serial: &str) -> Result<(), ConfigError> {
        let devices = self.loaded_configs.read().unwrap();

        if let Some(device) = devices.get(serial).cloned() {
            self.update_collection(&device);
            let mut path = self.device_config_path();
            fs::create_dir_all(&path).ok();
            path.push(format!("{}.json", serial));

            fs::write(path, serde_json::to_string(device.read().unwrap().deref()).unwrap())?;
            Ok(())
        } else {
            Err(ConfigError::DeviceNotFound)
        }
    }

    /// Saves device configs for all serials
    pub fn save_device_configs(&self) -> Result<(), ConfigError> {
        let devices = self.loaded_configs.read().unwrap();

        let path = self.device_config_path();
        fs::create_dir_all(&path).ok();

        for (serial, device) in devices.iter() {
            let device= device.clone();
            self.update_collection(&device);
            let mut file_path = path.clone();
            file_path.push(format!("{}.json", serial));
            fs::write(file_path, serde_json::to_string(device.read().unwrap().deref()).unwrap())?;
        }

        Ok(())
    }

    /// Retrieves device config for specified serial
    pub fn get_device_config(&self, serial: &str) -> Option<UniqueDeviceConfig> {
        self.loaded_configs.read().unwrap().get(serial).cloned()
    }

    /// Sets device config for specified serial
    pub fn set_device_config(&self, serial: &str, config: DeviceConfig) {
        let mut handle = self.loaded_configs.write().unwrap();

        if let Some(device_config) = handle.get(serial) {
            *device_config.write().unwrap() = config;
        } else {
            handle.insert(serial.to_string(), Arc::new(RwLock::new(config)));
        }
    }

    /// Gets an array of all device configs
    pub fn get_all_device_configs(&self) -> Vec<UniqueDeviceConfig> {
        self.loaded_configs.read().unwrap().values().map(|x| x.clone()).collect()
    }

    /// Disables a device config, so it will not be loaded by default
    pub fn disable_device_config(&self, serial: &str) -> bool {
        let path = self.device_config_path();

        let mut initial_path = path.clone();
        initial_path.push(format!("{}.json", serial));

        let mut new_path = path.clone();
        new_path.push(format!("{}.json_disabled", serial));

        fs::rename(initial_path, new_path).is_ok()
    }

    /// Restores device config if it exists
    pub fn restore_device_config(&self, serial: &str) -> bool {
        let path = self.device_config_path();

        let mut initial_path = path.clone();
        initial_path.push(format!("{}.json_disabled", serial));

        let mut new_path = path.clone();
        new_path.push(format!("{}.json", serial));

        fs::rename(initial_path, new_path).is_ok()
    }

    /// Adds base64 image to device config image collection
    pub fn add_image(&self, serial: &str, image: String) -> Option<String> {
        if let Some(config) = self.get_device_config(serial) {
            let mut config_handle = config.write().unwrap();
            let identifier = hash_str(&image);

            if let Ok(image) = SDImage::from_base64(&image, config_handle.kind().image_size()) {
                config_handle.images.insert(identifier.clone(), image.into());
                drop(config_handle);

                self.update_collection(&config);
                Some(identifier)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Encodes image to base64 and adds it to device config image collection
    pub fn add_image_encode(&self, serial: &str, image: DynamicImage) -> Option<String> {
        if let Some(config) = self.get_device_config(serial) {
            let mut config_handle = config.write().unwrap();
            let serialized_image = SDImage::SingleImage(resize_for_streamdeck(config_handle.kind().image_size(), image)).into();
            let identifier = hash_image(&serialized_image);
            config_handle.images.insert(identifier.clone(), serialized_image);
            drop(config_handle);

            self.update_collection(&config);
            return Some(identifier);
        }

        None
    }

    /// Gets images from device config
    pub fn get_images(&self, serial: &str) -> Option<HashMap<String, SDSerializedImage>> {
        if let Some(config) = self.get_device_config(serial) {
            let config_handle = config.read().unwrap();
            Some(config_handle.images.clone())
        } else {
            None
        }
    }

    /// Removes image from device config
    pub fn remove_image(&self, serial: &str, identifier: &str) -> bool {
        if let Some(config) = self.get_device_config(serial) {
            let mut config_handle = config.write().unwrap();
            config_handle.images.remove(identifier);
            drop(config_handle);

            self.remove_from_collection(serial, identifier);
            true
        } else {
            false
        }
    }

    /// Syncs images with core
    pub fn sync_images(&self, serial: &str) {
        if let Some(config) = self.get_device_config(serial) {
            self.update_collection(&config);
        }
    }

    /// Retrieves image collection for device if device exists
    pub fn get_image_collection(&self, serial: &str) -> ImageCollection {
        let mut handle = self.loaded_images.write().unwrap();

        if let Some(collection) = handle.get(serial) {
            collection.clone()
        } else {
            let collection: ImageCollection = Default::default();
            handle.insert(serial.to_string(), collection.clone());
            collection
        }
    }

    /// For making sure image collections strictly follow device config
    fn update_collection(&self, device_config: &UniqueDeviceConfig) {
        let mut device_config = device_config.write().unwrap();
        let mut handle = self.loaded_images.write().unwrap();

        if let Some(collection) = handle.get_mut(&device_config.serial) {
            let mut collection_handle = collection.write().unwrap();

            // Adding missing images from device config
            for (key, image) in &device_config.images {
                if !collection_handle.contains_key(key) {
                    if let Ok(image) = image.try_into() {
                        collection_handle.insert(key.to_string(), image);
                    }
                }
            }

            // Adding any images in collection to device config
            for (key, image) in collection_handle.iter() {
                if !device_config.images.contains_key(key) {
                    device_config.images.insert(key.to_string(), image.into());
                }
            }
        }
    }

    /// For removing images from image collections
    fn remove_from_collection(&self, serial: &str, identifier: &str) {
        let mut handle = self.loaded_images.write().unwrap();

        if let Some(collection) = handle.get_mut(serial) {
            let mut collection_handle = collection.write().unwrap();
            collection_handle.remove(identifier);
        }
    }
}

/// Plugin Config trait for serialization and deserialization methods
pub trait PluginConfig {
    const NAME: &'static str;
}

/// Error enum for various errors while loading and parsing configs
#[derive(Debug)]
pub enum ConfigError {
    IoError(std::io::Error),
    ParseError(serde_json::Error),
    DeviceNotFound
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        ConfigError::IoError(err)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(err: serde_json::Error) -> Self {
        ConfigError::ParseError(err)
    }
}

/// Device config struct
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DeviceConfig {
    pub vid: u16,
    pub pid: u16,
    pub serial: String,
    pub brightness: u8,
    pub layout: RawButtonPanel,
    pub images: HashMap<String, SDSerializedImage>,
    pub plugin_data: HashMap<String, Value>,
}

impl DeviceConfig {
    /// Gets kind of the device
    pub fn kind(&self) -> Kind {
        match self.pid {
            streamdeck::pids::ORIGINAL_V2 => Kind::OriginalV2,
            streamdeck::pids::MINI => Kind::Mini,
            streamdeck::pids::MK2 => Kind::Mk2,
            streamdeck::pids::XL => Kind::Xl,

            _ => Kind::Original,
        }
    }
}