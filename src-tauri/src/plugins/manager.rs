use crate::plugins::PluginLoader;
use libloading::{Library, Symbol};
use plugin_interfaces::{
    log_error, log_info,
    pluginui::{Context, Ui},
    CreatePluginFn, DestroyPluginFn, HostCallbacks, PluginInterface, PluginMetadata,
    CREATE_PLUGIN_SYMBOL, DESTROY_PLUGIN_SYMBOL,
};
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter};

// 全局AppHandle存储，用于在回调函数中访问
static GLOBAL_APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// 插件实例信息
pub struct PluginInstance {
    pub metadata: PluginMetadata,
    pub handler: *mut PluginInterface,
    pub library: Library,
    pub is_mounted: bool,
    pub is_connected: bool,
    pub ui_data: Option<String>,             // 保存序列化的UI数据
    pub ui_instance: Option<Arc<Mutex<Ui>>>, // 保存UI实例以处理事件
}

impl std::fmt::Debug for PluginInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginInstance")
            .field("metadata", &self.metadata)
            .field("is_mounted", &self.is_mounted)
            .field("is_connected", &self.is_connected)
            .field("has_ui_data", &self.ui_data.is_some())
            .field("has_ui_instance", &self.ui_instance.is_some())
            .finish()
    }
}

unsafe impl Send for PluginInstance {}
unsafe impl Sync for PluginInstance {}

/// 插件管理器
#[derive(Debug)]
pub struct PluginManager {
    loader: PluginLoader,
    instances: Arc<Mutex<HashMap<String, PluginInstance>>>,
    current_plugin: Arc<Mutex<Option<String>>>,
    app_handle: AppHandle,
}

impl PluginManager {
    pub fn new(app_handle: AppHandle) -> Self {
        Self {
            loader: PluginLoader::new(),
            instances: Arc::new(Mutex::new(HashMap::new())),
            current_plugin: Arc::new(Mutex::new(None)),
            app_handle,
        }
    }

    /// 创建主程序回调函数集合
    fn create_host_callbacks(&self) -> HostCallbacks {
        // 将AppHandle克隆并存储在静态变量中，供回调函数使用
        GLOBAL_APP_HANDLE.set(self.app_handle.clone()).ok();

        HostCallbacks {
            send_to_frontend: Self::host_send_to_frontend,
            get_app_config: Self::host_get_app_config,
            call_other_plugin: Self::host_call_other_plugin,
        }
    }

    /// 向前端发送消息
    extern "C" fn host_send_to_frontend(event: *const c_char, payload: *const c_char) -> bool {
        if !event.is_null() && !payload.is_null() {
            unsafe {
                if let (Ok(event_str), Ok(payload_str)) = (
                    CStr::from_ptr(event).to_str(),
                    CStr::from_ptr(payload).to_str(),
                ) {
                    // 实现实际的Tauri事件发送
                    if let Some(app_handle) = GLOBAL_APP_HANDLE.get() {
                        match app_handle.emit(event_str, payload_str) {
                            Ok(_) => {
                                return true;
                            }
                            Err(e) => {
                                log_error!(
                                    "[PLUGIN->FRONTEND] Failed to send event {}: {}",
                                    event_str,
                                    e
                                );
                            }
                        }
                    } else {
                        log_error!("[PLUGIN->FRONTEND] AppHandle not available");
                    }
                }
            }
        }
        false
    }

    /// 获取应用配置
    extern "C" fn host_get_app_config(key: *const c_char) -> *const c_char {
        if !key.is_null() {
            unsafe {
                if let Ok(key_str) = CStr::from_ptr(key).to_str() {
                    // TODO: 实现实际的配置获取逻辑
                    let config_value = format!("config_value_for_{}", key_str);
                    if let Ok(c_string) = CString::new(config_value) {
                        return c_string.into_raw();
                    }
                }
            }
        }
        std::ptr::null()
    }

    /// 调用其他插件
    extern "C" fn host_call_other_plugin(
        plugin_id: *const c_char,
        message: *const c_char,
    ) -> *const c_char {
        if !plugin_id.is_null() && !message.is_null() {
            unsafe {
                if let (Ok(id_str), Ok(_msg_str)) = (
                    CStr::from_ptr(plugin_id).to_str(),
                    CStr::from_ptr(message).to_str(),
                ) {
                    // TODO: 实现实际的插件间通信逻辑
                    let response = format!("response_from_{}", id_str);
                    if let Ok(c_string) = CString::new(response) {
                        return c_string.into_raw();
                    }
                }
            }
        }
        std::ptr::null()
    }

    /// 扫描插件列表
    pub fn scan_plugins(&self) -> Vec<PluginMetadata> {
        self.loader.scan_plugins()
    }

    /// 挂载插件
    pub fn mount_plugin(&self, plugin_id: &str) -> Result<String, String> {
        // 获取当前插件ID
        let current_plugin_id = {
            let current = self.current_plugin.lock().unwrap();
            current.clone()
        };
        // 先卸载当前插件
        if let Some(current_id) = current_plugin_id {
            if current_id != plugin_id {
                if let Err(e) = self.dispose_plugin(&current_id) {
                    log_error!("Failed to dispose current plugin: {}", e);
                }
            }
        }

        let mut instances = self.instances.lock().unwrap();

        // 如果插件已经存在且已挂载，直接返回成功
        if let Some(instance) = instances.get(plugin_id) {
            if instance.is_mounted {
                *self.current_plugin.lock().unwrap() = Some(plugin_id.to_string());
                return Ok(format!("插件 {} 已经挂载", instance.metadata.name));
            }
        }

        // 加载插件
        let plugin_metadata = self.find_plugin_metadata(plugin_id)?;
        let library_path = plugin_metadata
            .library_path
            .as_ref()
            .ok_or_else(|| format!("插件 {} 没有找到动态库文件", plugin_id))?;

        // 动态加载库
        let library = unsafe {
            Library::new(library_path).map_err(|e| format!("加载动态库失败: {}", e))?
        };

        // 获取创建函数
        let create_plugin: Symbol<CreatePluginFn> = unsafe {
            library
                .get(CREATE_PLUGIN_SYMBOL)
                .map_err(|e| format!("找不到插件创建函数: {}", e))?
        };

        // 创建插件实例
        let handler = unsafe { create_plugin() };
        if handler.is_null() {
            return Err("插件创建失败".to_string());
        }

        // 初始化插件（设置回调函数）
        let callbacks = self.create_host_callbacks();
        let init_result = unsafe { ((*handler).initialize)((*handler).plugin_ptr, callbacks) };
        if init_result != 0 {
            // 清理失败的插件实例
            unsafe {
                let destroy_plugin: Result<Symbol<DestroyPluginFn>, _> =
                    library.get(DESTROY_PLUGIN_SYMBOL);
                if let Ok(destroy_fn) = destroy_plugin {
                    destroy_fn(handler);
                }
            }
            return Err("插件初始化失败".to_string());
        }

        // 调用 on_mount，传递元数据
        let metadata_ffi = plugin_metadata.to_ffi();
        let mount_result = unsafe { ((*handler).on_mount)((*handler).plugin_ptr, metadata_ffi) };

        // 清理FFI元数据内存
        unsafe {
            plugin_interfaces::metadata::free_plugin_metadata_ffi(metadata_ffi);
        }

        let result: Result<(), Box<dyn std::error::Error>> = if mount_result == 0 {
            Ok(())
        } else {
            Err("插件挂载失败".into())
        };

        // 初始化UI
        let context = Context::new(plugin_id.to_string());
        let ui_arc = Ui::new(plugin_id.to_string());
        let mut ui = ui_arc.lock().unwrap();

        // 保存UI实例的引用以便后续事件处理
        let ui_instance_ref = Arc::clone(&ui_arc);

        unsafe {
            ((*handler).update_ui)(
                (*handler).plugin_ptr,
                &context as *const Context as *const std::ffi::c_void,
                &mut *ui as *mut Ui as *mut std::ffi::c_void,
            )
        };

        let ui_data = match serde_json::to_string(&ui.get_components()) {
            Ok(json) => json,
            Err(e) => {
                log_error!("序列化UI数据失败: {}", e);
                "[]".to_string()
            }
        };

        match result {
            Ok(_) => {
                // 创建插件实例
                let instance = PluginInstance {
                    metadata: plugin_metadata.clone(),
                    handler,
                    library,
                    is_mounted: true,
                    is_connected: false,
                    ui_data: Some(ui_data),
                    ui_instance: Some(ui_instance_ref),
                };

                instances.insert(plugin_id.to_string(), instance);
                *self.current_plugin.lock().unwrap() = Some(plugin_id.to_string());

                Ok(format!("插件 {} 挂载成功", plugin_metadata.name))
            }
            Err(e) => {
                // 清理失败的插件实例
                unsafe {
                    let destroy_plugin: Result<Symbol<DestroyPluginFn>, _> =
                        library.get(DESTROY_PLUGIN_SYMBOL);
                    if let Ok(destroy_fn) = destroy_plugin {
                        destroy_fn(handler);
                    }
                }
                Err(format!("插件挂载失败: {}", e))
            }
        }
    }

    /// 卸载插件
    pub fn dispose_plugin(&self, plugin_id: &str) -> Result<String, String> {
        let mut instances = self.instances.lock().unwrap();

        if let Some(instance) = instances.get_mut(plugin_id) {
            if !instance.is_mounted {
                return Ok(format!("插件 {} 已经卸载", instance.metadata.name));
            }

            // 先断开连接
            if instance.is_connected {
                let _ =
                    unsafe { ((*instance.handler).on_disconnect)((*instance.handler).plugin_ptr) };
                instance.is_connected = false;
            }

            // 调用 on_dispose
            let dispose_result =
                unsafe { ((*instance.handler).on_dispose)((*instance.handler).plugin_ptr) };
            let result: Result<(), Box<dyn std::error::Error>> = if dispose_result == 0 {
                Ok(())
            } else {
                Err("插件卸载失败".into())
            };

            // 销毁插件实例
            unsafe {
                let destroy_plugin: Result<Symbol<DestroyPluginFn>, _> =
                    instance.library.get(DESTROY_PLUGIN_SYMBOL);
                if let Ok(destroy_fn) = destroy_plugin {
                    destroy_fn(instance.handler);
                }
            }

            instance.is_mounted = false;

            // 如果这是当前插件，清除当前插件状态
            let mut current = self.current_plugin.lock().unwrap();
            if current.as_ref() == Some(&plugin_id.to_string()) {
                *current = None;
            }

            match result {
                Ok(_) => Ok(format!("插件 {} 卸载成功", instance.metadata.name)),
                Err(e) => Ok(format!(
                    "插件 {} 卸载完成，但有警告: {}",
                    instance.metadata.name, e
                )),
            }
        } else {
            Err(format!("插件 {} 未找到", plugin_id))
        }
    }

    /// 连接插件
    pub fn connect_plugin(&self, plugin_id: &str) -> Result<String, String> {
        let mut instances = self.instances.lock().unwrap();

        if let Some(instance) = instances.get_mut(plugin_id) {
            if !instance.is_mounted {
                return Err(format!("插件 {} 未挂载", instance.metadata.name));
            }

            if instance.is_connected {
                return Ok(format!("插件 {} 已经连接", instance.metadata.name));
            }

            let connect_result =
                unsafe { ((*instance.handler).on_connect)((*instance.handler).plugin_ptr) };
            let result: Result<(), Box<dyn std::error::Error>> = if connect_result == 0 {
                Ok(())
            } else {
                Err("插件连接失败".into())
            };

            match result {
                Ok(_) => {
                    instance.is_connected = true;
                    Ok(format!("插件 {} 连接成功", instance.metadata.name))
                }
                Err(e) => Err(format!("插件连接失败: {}", e)),
            }
        } else {
            Err(format!("插件 {} 未找到", plugin_id))
        }
    }

    /// 断开插件连接
    pub fn disconnect_plugin(&self, plugin_id: &str) -> Result<String, String> {
        let mut instances = self.instances.lock().unwrap();

        if let Some(instance) = instances.get_mut(plugin_id) {
            if !instance.is_mounted {
                return Err(format!("插件 {} 未挂载", instance.metadata.name));
            }

            if !instance.is_connected {
                return Ok(format!("插件 {} 已经断开连接", instance.metadata.name));
            }

            let disconnect_result =
                unsafe { ((*instance.handler).on_disconnect)((*instance.handler).plugin_ptr) };
            let result: Result<(), Box<dyn std::error::Error>> = if disconnect_result == 0 {
                Ok(())
            } else {
                Err("插件断开连接失败".into())
            };

            instance.is_connected = false;

            match result {
                Ok(_) => Ok(format!("插件 {} 断开连接成功", instance.metadata.name)),
                Err(e) => Ok(format!(
                    "插件 {} 断开连接完成，但有警告: {}",
                    instance.metadata.name, e
                )),
            }
        } else {
            Err(format!("插件 {} 未找到", plugin_id))
        }
    }

    /// 获取当前插件状态
    pub fn get_plugin_status(&self, plugin_id: &str) -> Option<(bool, bool)> {
        let instances = self.instances.lock().unwrap();
        instances
            .get(plugin_id)
            .map(|instance| (instance.is_mounted, instance.is_connected))
    }

    /// 获取当前活跃插件
    pub fn get_current_plugin(&self) -> Option<String> {
        self.current_plugin.lock().unwrap().clone()
    }

    /// 向当前活跃插件发送消息
    pub fn send_message_to_current_plugin(&self, message: &str) -> Result<String, String> {
        let instances = self.instances.lock().unwrap();
        let current_plugin_id = self.current_plugin.lock().unwrap();

        if let Some(plugin_id) = current_plugin_id.as_ref() {
            if let Some(instance) = instances.get(plugin_id) {
                if instance.is_mounted {
                    unsafe {
                        let message_cstr = CString::new(message).map_err(|_| "消息转换失败")?;
                        let mut result_ptr: *mut c_char = std::ptr::null_mut();
                        let handle_result = ((*instance.handler).handle_message)(
                            (*instance.handler).plugin_ptr,
                            message_cstr.as_ptr(),
                            &mut result_ptr,
                        );

                        if handle_result == 0 && !result_ptr.is_null() {
                            let response = CStr::from_ptr(result_ptr).to_string_lossy().to_string();
                            // 释放插件分配的内存
                            let _ = CString::from_raw(result_ptr);
                            Ok(response)
                        } else {
                            Err("插件处理消息失败".to_string())
                        }
                    }
                } else {
                    Err("当前插件未挂载".to_string())
                }
            } else {
                Err("当前插件未找到".to_string())
            }
        } else {
            Err("没有活跃的插件".to_string())
        }
    }

    /// 获取插件UI定义
    pub fn get_plugin_ui(&self, plugin_id: &str) -> Result<String, String> {
        let mut instances = self.instances.lock().unwrap();

        if let Some(instance) = instances.get_mut(plugin_id) {
            if instance.is_mounted {
                let ui_arc = instance.ui_instance.as_ref().ok_or("UI实例未找到")?;
                let ui = ui_arc.lock().unwrap();

                let ui_data = match serde_json::to_string(&ui.get_components()) {
                    Ok(json) => json,
                    Err(e) => {
                        log_error!("序列化UI数据失败: {}", e);
                        "[]".to_string()
                    }
                };
                instance.ui_data = Some(ui_data.clone());
                Ok(ui_data)
            } else {
                Err("插件未挂载".to_string())
            }
        } else {
            Err("插件未找到".to_string())
        }
    }

    pub fn handle_plugin_ui_update(
        &self,
        plugin_id: &str,
        component_id: &str,
        value: &str,
    ) -> Result<bool, String> {
        let mut instances = self.instances.lock().unwrap();

        if let Some(instance) = instances.get_mut(plugin_id) {
            if !instance.is_mounted {
                return Err("插件未挂载".to_string());
            }

            if let Some(ui_instance) = &instance.ui_instance {
                // 创建包含UI事件数据的Context
                let mut ui_event_data = std::collections::HashMap::new();
                ui_event_data.insert(component_id.to_string(), value.to_string());
                let context = Context::with_ui_event_data(plugin_id.to_string(), ui_event_data);
                let mut ui = ui_instance.lock().unwrap();

                // 只清除组件，保留事件状态用于本次update_ui
                ui.clear_components_only();

                let update_ui_result = unsafe {
                    ((*instance.handler).update_ui)(
                        (*instance.handler).plugin_ptr,
                        &context as *const Context as *const std::ffi::c_void,
                        &mut *ui as *mut Ui as *mut std::ffi::c_void,
                    )
                };

                if update_ui_result == 0 {
                    // 更新UI数据
                    let ui_data = match serde_json::to_string(&ui.get_components()) {
                        Ok(json) => json,
                        Err(e) => {
                            log_error!("序列化UI数据失败: {}", e);
                            "[]".to_string()
                        }
                    };
                    instance.ui_data = Some(ui_data.clone());

                    // 清除事件状态，为下次事件做准备
                    ui.clear_events();

                    drop(ui); // 释放锁
                    drop(instances); // 释放instances锁

                    // 发送UI更新事件到前端
                    let _ = self.notify_plugin_ui_update(plugin_id);
                }
                Ok(true)
            } else {
                Err("插件未找到".to_string())
            }
        } else {
            return Err("插件未找到".to_string());
        }
    }

    /// 处理插件UI事件
    pub fn handle_plugin_ui_event(
        &self,
        plugin_id: &str,
        component_id: &str,
        value: &str,
    ) -> Result<bool, String> {
        let mut instances = self.instances.lock().unwrap();

        if let Some(instance) = instances.get_mut(plugin_id) {
            if instance.is_mounted {
                let mut event_handled = false;

                // 首先尝试使用UI实例处理事件
                if let Some(ui_instance) = &instance.ui_instance {
                    if let Ok(mut ui) = ui_instance.lock() {
                        let handled = ui.handle_ui_event(component_id, value);
                        if handled {
                            event_handled = true;
                        }
                    }
                }

                // 如果事件被处理，调用 update_ui 并发送更新事件
                if event_handled {
                    if let Some(ui_instance) = &instance.ui_instance {
                        // 创建包含UI事件数据的Context
                        let mut ui_event_data = std::collections::HashMap::new();
                        ui_event_data.insert(component_id.to_string(), value.to_string());
                        let context =
                            Context::with_ui_event_data(plugin_id.to_string(), ui_event_data);
                        let mut ui = ui_instance.lock().unwrap();

                        // 确保UI实例也有事件数据（这是关键！）
                        ui.handle_ui_event(component_id, value);

                        // 只清除组件，保留事件状态用于本次update_ui
                        ui.clear_components_only();

                        let update_ui_result = unsafe {
                            ((*instance.handler).update_ui)(
                                (*instance.handler).plugin_ptr,
                                &context as *const Context as *const std::ffi::c_void,
                                &mut *ui as *mut Ui as *mut std::ffi::c_void,
                            )
                        };

                        if update_ui_result == 0 {
                            // 更新UI数据
                            let ui_data = match serde_json::to_string(&ui.get_components()) {
                                Ok(json) => json,
                                Err(e) => {
                                    log_error!("序列化UI数据失败: {}", e);
                                    "[]".to_string()
                                }
                            };
                            instance.ui_data = Some(ui_data.clone());

                            // 清除事件状态，为下次事件做准备
                            ui.clear_events();

                            drop(ui); // 释放锁
                            drop(instances); // 释放instances锁

                            // 发送UI更新事件到前端
                            let _ = self.notify_plugin_ui_update(plugin_id);
                        }
                    }
                }

                Ok(event_handled)
            } else {
                Err("插件未挂载".to_string())
            }
        } else {
            Err("插件未找到".to_string())
        }
    }

    /// 通知插件UI更新
    pub fn notify_plugin_ui_update(&self, plugin_id: &str) -> Result<(), String> {
        // 向前端发送UI更新事件，使用 host_send_to_frontend 而不是直接调用 app_handle.emit
        let payload = serde_json::json!({
            "plugin": plugin_id
        });
        let payload_str = payload.to_string();

        // 使用 CString 确保字符串以 null 结尾
        let event_cstr = CString::new("plugin-ui-updated").map_err(|_| "事件名称转换失败")?;
        let payload_cstr = CString::new(payload_str).map_err(|_| "载荷转换失败")?;

        if Self::host_send_to_frontend(event_cstr.as_ptr(), payload_cstr.as_ptr()) {
            Ok(())
        } else {
            Err("发送UI更新事件失败".to_string())
        }
    }

    /// 清理所有已挂载的插件（应用关闭时调用）
    pub fn cleanup_all_plugins(&self) {
        let mut instances = self.instances.lock().unwrap();

        // 收集所有已挂载的插件ID
        let mounted_plugin_ids: Vec<String> = instances
            .iter()
            .filter(|(_, instance)| instance.is_mounted)
            .map(|(id, _)| id.clone())
            .collect();

        // 逐个清理插件
        for plugin_id in mounted_plugin_ids {
            if let Some(instance) = instances.get_mut(&plugin_id) {
                if instance.is_mounted {
                    log_info!("正在清理插件: {}", instance.metadata.name);

                    // 先断开连接
                    if instance.is_connected {
                        let _ = unsafe {
                            ((*instance.handler).on_disconnect)((*instance.handler).plugin_ptr)
                        };
                        instance.is_connected = false;
                    }

                    // 调用 on_dispose
                    let _ =
                        unsafe { ((*instance.handler).on_dispose)((*instance.handler).plugin_ptr) };

                    // 销毁插件实例
                    unsafe {
                        let destroy_plugin: Result<Symbol<DestroyPluginFn>, _> =
                            instance.library.get(DESTROY_PLUGIN_SYMBOL);
                        if let Ok(destroy_fn) = destroy_plugin {
                            destroy_fn(instance.handler);
                        }
                    }

                    instance.is_mounted = false;
                    log_info!("插件 {} 清理完成", instance.metadata.name);
                }
            }
        }

        // 清除当前插件状态
        *self.current_plugin.lock().unwrap() = None;

        log_info!("所有插件清理完成");
    }

    /// 查找插件元数据
    fn find_plugin_metadata(&self, plugin_id: &str) -> Result<PluginMetadata, String> {
        let plugins = self.scan_plugins();
        plugins
            .into_iter()
            .find(|p| p.id == plugin_id)
            .ok_or_else(|| format!("插件 {} 未找到", plugin_id))
    }
}
