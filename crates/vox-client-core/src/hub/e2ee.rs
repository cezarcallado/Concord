//! End-to-end encryption, E0 (v1.1): the directory calls of [`HubClient`] (the contract is in
//! `vox_proto::e2ee`). Only made to a hub whose `Ready` said `e2ee` (an older hub never sees them).
//! Keys are never logged, nor put in an error text.

use vox_e2ee::Identity;
use vox_proto::{E2eeDevice, E2eeDeviceRegister, ERR_E2EE_KEY_CHANGED, ERR_E2EE_TOO_MANY_DEVICES};

use super::*;

/// How the hub answered a registration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterReply {
    /// Registered (or confirmed: same id, same key).
    Registered(E2eeDevice),
    /// `409`: this device id is registered with another key (a reinstall reused it): register
    /// again under a new id.
    KeyChanged,
    /// `409`: the account already has the most devices.
    TooMany,
    /// `404` / `405`: the hub has no directory after all.
    Unsupported,
    /// `429`: too many changes just now.
    Busy,
    Failed(String),
}

impl HubClient {
    /// `GET /api/e2ee/devices`: every member's devices.
    pub async fn e2ee_devices(&self) -> Result<Vec<E2eeDevice>> {
        let resp = self.auth(self.http.get(self.url("api/e2ee/devices")?))?.send().await.context("e2ee devices request")?;
        Self::parse("e2ee devices", resp).await
    }

    /// `PUT /api/e2ee/devices/{id}`: register this device (its public key and `label`).
    pub async fn register_e2ee_device(&self, identity: &Identity, label: &str) -> RegisterReply {
        let body = E2eeDeviceRegister { public_key: vox_e2ee::public_key_to_b64(&identity.public_key()), label: vox_proto::fit_e2ee_label(label) };
        let req = match self.url(&format!("api/e2ee/devices/{}", identity.device_id_hex())).and_then(|u| self.auth(self.http.put(u))) {
            Ok(r) => r,
            Err(e) => return RegisterReply::Failed(format!("{e:#}")),
        };
        let resp = match req.json(&body).send().await {
            Ok(r) => r,
            Err(e) => return RegisterReply::Failed(format!("e2ee register: {e}")),
        };
        match resp.status().as_u16() {
            s if (200..300).contains(&s) => match resp.json::<E2eeDevice>().await {
                Ok(d) => RegisterReply::Registered(d),
                Err(e) => RegisterReply::Failed(format!("e2ee register: invalid response body: {e}")),
            },
            409 => {
                let text = resp.text().await.unwrap_or_default();
                match serde_json::from_str::<ApiError>(&text).map(|e| e.error) {
                    Ok(e) if e == ERR_E2EE_KEY_CHANGED => RegisterReply::KeyChanged,
                    Ok(e) if e == ERR_E2EE_TOO_MANY_DEVICES => RegisterReply::TooMany,
                    _ => RegisterReply::Failed("e2ee register: HTTP 409".into()),
                }
            }
            404 | 405 => RegisterReply::Unsupported,
            429 => RegisterReply::Busy,
            s => RegisterReply::Failed(format!("e2ee register: HTTP {s}")),
        }
    }

    /// `DELETE /api/e2ee/devices/{id}`: remove one of my devices. `Ok(false)`: I have no such device
    /// (any more).
    pub async fn delete_e2ee_device(&self, device_id: &str) -> Result<bool> {
        if !vox_proto::is_e2ee_device_id(device_id) {
            bail!("not a device id");
        }
        let resp = self.auth(self.http.delete(self.url(&format!("api/e2ee/devices/{device_id}"))?))?.send().await.context("e2ee device delete")?;
        match resp.status().as_u16() {
            s if (200..300).contains(&s) => Ok(true),
            404 => Ok(false),
            429 => bail!("e2ee device delete: too many changes, try again shortly"),
            s => bail!("e2ee device delete: HTTP {s}"),
        }
    }
}
