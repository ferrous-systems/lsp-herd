//! ## Language server protocol
//!
//! Specification can be found at
//! <https://microsoft.github.io/language-server-protocol/specifications/specification-current/>.
//!
//! We're not interested in supporting or even parsing the whole protocol, we only want a subset
//! that will allow us to mupltiplex messages between multiple clients and a single server.
//!
//! LSP has several main message types:
//!
//! ### Request Message
//! Requests from client to server. Requests contain an `id` property which is either `integer` or
//! `string`.
//!
//! ### Response Message
//! Responses from server for client requests. Also contain an `id` property, but according to the
//! the specification it can also be null, it's unclear what we should do when it is null. We could
//! either send the response to all clients or drop it.
//!
//! ### Notification Message
//! Notifications must not receive a response, this doesn't really mean anything to us as we're
//! just relaying the messages. It sounds like it'd allow us to simply pass a notification from any
//! client to the server and to pass a server notification to all clients, however there are some
//! subtypes of notifications defined by the LSP where that could be confusing to the client or
//! server:
//! - Cancel notifications - contains an `id` property again, so we could multiplex this like any
//!   other request
//! - Progress notifications - contains a `token` property which could be used to identify the
//!   client but the specification also says it has nothing to do with the request IDs

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

macro_rules! impl_json_debug {
    ( $($type:ty),* $(,)? ) => {
        $(
            impl ::std::fmt::Debug for $type {
                fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
                    let json = ::serde_json::to_string(self).expect("BUG: invalid message");
                    f.write_str(&json)
                }
            }
        )*
    };
}

pub mod ext;
pub mod jsonrpc;
pub mod transport;

/// Params for the `initialize` request
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// > The process Id of the parent process that started the server. Is
    /// > null if the process has not been started by another process. If the
    /// > parent process is not alive then the server should exit (see exit
    /// > notification) its process.
    pub process_id: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ClientInfo>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_path: Option<String>,

    pub root_uri: Option<String>,

    /// User provided initialization options
    ///
    /// This is where lspmux-proxy should be inserting it's setup. However we
    /// should remove them again before forwarding them to the language server.
    pub initialization_options: Option<InitializationOptions>,

    pub capabilities: Option<serde_json::Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceValue>,

    /// > The workspace folders configured in the client when the server starts.
    /// >
    /// > This property is only available if the client supports workspace
    /// > folders. It can be `null` if the client supports workspace folders but
    /// > none are configured.
    ///
    /// We map null to an empty vec (indicating no folders are configured), and
    /// absent to None (indicating workspace folders are unsupported).
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        deserialize_with = "deserialize_workspace_folders"
    )]
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
}

fn deserialize_workspace_folders<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<WorkspaceFolder>>, D::Error>
where
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(|v: Option<Vec<_>>| Some(v.unwrap_or_default()))
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitializationOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsp_mux: Option<ext::LspMuxOptions>,

    #[serde(flatten)]
    pub other_options: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub enum TraceValue {
    Off,
    Messages,
    Verbose,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFolder {
    pub uri: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    capabilities: serde_json::Value,

    #[serde(skip_serializing_if = "Option::is_none")]
    server_info: Option<ServerInfo>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ServerInfo {
    name: String,
    version: Option<String>,
}

/// Params for `client/registerCapability` request
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RegistrationParams {
    pub registrations: Vec<Registration>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub id: String,
    pub method: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub register_options: Option<serde_json::Value>,
}

/// Params for `client/unregisterCapability` request
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct UnregistrationParams {
    /// This is a typo in the LSP 3.xx spec, we need to replicate it until they
    /// upgrade the protocol version.
    #[serde(rename = "unregisterations")]
    pub unregistrations: Vec<Unregistration>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Unregistration {
    pub id: String,
    pub method: String,
}

/// Params for `textDocument/didOpen` notification
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    pub text_document: TextDocumentItem,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    pub uri: String,
    pub language_id: String,
    pub version: u64,
    pub text: String,
}

/// Params for `textDocument/didClose` notification
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DidCloseTextDocumentParams {
    pub text_document: TextDocumentIdentifier,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

/// Params for `textDocument/didChange` notification
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    pub text_document: VersionedTextDocumentIdentifier,
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct VersionedTextDocumentIdentifier {
    pub uri: String,
    pub version: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentContentChangeEvent {
    pub text: String,
}

/// Parse LSP `URI` type as an absolute file path.
pub fn parse_file_uri(root_uri: &str) -> Result<String> {
    use percent_encoding::percent_decode_str;
    use uriparse::URI;

    let (scheme, _, mut path, _, _) = URI::try_from(root_uri)
        .map_err(|e| anyhow::anyhow!("failed to parse URI: {}", e))?
        .into_parts();

    if scheme != uriparse::Scheme::File {
        anyhow::bail!("only `file://` URIs are supported");
    }

    path.normalize(false);

    let path = percent_decode_str(&path.to_string())
        .decode_utf8()
        .map_err(|e| anyhow::anyhow!("decoded URI was not valid utf-8: {}", e))?
        .to_string();

    // On windows the drive letter `C:/` gets interpreted as the first
    // segment of an absolute path which results in an extra `/` at the
    // beginning of the string representation which needs to be removed.
    let path = match path.as_bytes() {
        #[cfg(any(windows, test))]
        [b'/', drive, b':', b'/', ..] if drive.is_ascii_alphabetic() => {
            path.strip_prefix('/').unwrap().to_owned()
        }
        _ => path,
    };

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{parse_file_uri as p, InitializeParams};

    #[test]
    fn parsing_file_uris() {
        assert_eq!(p("file:///home/user/proj").unwrap(), "/home/user/proj");
        assert_eq!(p("file:///c:/dev/proj").unwrap(), "c:/dev/proj");
        assert_eq!(p("file:///proj").unwrap(), "/proj");
        assert_eq!(p("file:///d:/proj").unwrap(), "d:/proj");
        assert_eq!(p("file:///").unwrap(), "/");
        assert_eq!(p("file:///e:/").unwrap(), "e:/");
    }

    #[test]
    #[allow(non_snake_case)]
    fn deserialize_InitializeParams_workspace_folders() {
        assert!(matches!(
            serde_json::from_str(r"{}"),
            Ok(InitializeParams {
                workspace_folders: None,
                ..
            })
        ));
        assert!(matches!(
            serde_json::from_str(r#"{"workspaceFolders": null}"#),
            Ok(InitializeParams {
                workspace_folders: Some(workspace_folders),
                ..
            }) if workspace_folders.is_empty()
        ));
        assert!(matches!(
            serde_json::from_str(r#"{"workspaceFolders": []}"#),
            Ok(InitializeParams {
                workspace_folders: Some(workspace_folders),
                ..
            }) if workspace_folders.is_empty()
        ));
    }
}
