use serde::{Deserialize, Serialize};
use spin_app::{App, APP_NAME_KEY, APP_VERSION_KEY, BINDLE_VERSION_KEY, OCI_IMAGE_DIGEST_KEY, ORIGIN_KEY};

#[derive(Debug, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindle_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oci_image_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

impl AppInfo {
    pub fn new(app: &App) -> Self {
        let name = app
            .get_metadata(APP_NAME_KEY)
            .unwrap_or_default()
            .unwrap_or_default();
        let version = app.get_metadata(APP_VERSION_KEY).unwrap_or_default();
        let bindle_version = app.get_metadata(BINDLE_VERSION_KEY).unwrap_or_default();
        let oci_image_digest = app.get_metadata(OCI_IMAGE_DIGEST_KEY).unwrap_or_default();
        let origin = app.get_metadata(ORIGIN_KEY).unwrap_or_default();
        Self {
            name,
            version,
            bindle_version,
            oci_image_digest,
            origin
        }
    }
}
