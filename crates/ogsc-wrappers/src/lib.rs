//! Compatibility runtime wrapper catalog, config generation, and transactional deployment
//! (`dgVoodoo 2`, `DxWrapper`, `WineD3D`, `XInput Plus`, `DOSBox-X`, `86Box`) for **Old Games – Second Chance**.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod config;
pub mod deploy;
pub mod error;

pub use catalog::{ToolkitAvailability, ToolkitDownloadSpec, ToolkitKind, ToolkitStore};
pub use config::{
    render_86box_cfg, render_dgvoodoo_conf, render_dosbox_x_conf, render_dxwrapper_ini,
    render_xinput_plus_ini,
};
pub use deploy::{
    deploy_wrapper, plan_wrapper_deployment, PlannedConfigWrite, PlannedDllCopy,
    WrapperDeployReport, WrapperPlan,
};
pub use error::WrapperError;
