pub mod changes;
pub mod copy;
pub mod get;
pub mod query;
pub mod query_changes;
pub mod set;

use store::{chrono::DateTime, DocumentId};

use crate::{
    id::{blob::JMAPBlob, jmap::JMAPId},
    protocol::{json::JSONValue, json_pointer::JSONPointer, response::Response},
    MethodError,
};

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ResultReference {
    #[serde(rename = "resultOf")]
    pub result_of: String,
    pub name: Method,
    pub path: JSONPointer,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum MaybeResultReference<T> {
    Value(T),
    Reference(ResultReference),
}

#[derive(Debug, Clone)]
pub enum MaybeIdReference {
    Value(JMAPId),
    Reference(String),
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum Method {
    #[serde(rename = "Core/echo")]
    Echo,
    #[serde(rename = "Blob/copy")]
    CopyBlob,
    #[serde(rename = "PushSubscription/get")]
    GetPushSubscription,
    #[serde(rename = "PushSubscription/set")]
    SetPushSubscription,
    #[serde(rename = "Mailbox/get")]
    GetMailbox,
    #[serde(rename = "Mailbox/changes")]
    ChangesMailbox,
    #[serde(rename = "Mailbox/query")]
    QueryMailbox,
    #[serde(rename = "Mailbox/queryChanges")]
    QueryChangesMailbox,
    #[serde(rename = "Mailbox/set")]
    SetMailbox,
    #[serde(rename = "Thread/get")]
    GetThread,
    #[serde(rename = "Thread/changes")]
    ChangesThread,
    #[serde(rename = "Email/get")]
    GetEmail,
    #[serde(rename = "Email/changes")]
    ChangesEmail,
    #[serde(rename = "Email/query")]
    QueryEmail,
    #[serde(rename = "Email/queryChanges")]
    QueryChangesEmail,
    #[serde(rename = "Email/set")]
    SetEmail,
    #[serde(rename = "Email/copy")]
    CopyEmail,
    #[serde(rename = "Email/import")]
    ImportEmail,
    #[serde(rename = "Email/parse")]
    ParseEmail,
    #[serde(rename = "SearchSnippet/get")]
    GetSearchSnippet,
    #[serde(rename = "Identity/get")]
    GetIdentity,
    #[serde(rename = "Identity/changes")]
    ChangesIdentity,
    #[serde(rename = "Identity/set")]
    SetIdentity,
    #[serde(rename = "EmailSubmission/get")]
    GetEmailSubmission,
    #[serde(rename = "EmailSubmission/changes")]
    ChangesEmailSubmission,
    #[serde(rename = "EmailSubmission/query")]
    QueryEmailSubmission,
    #[serde(rename = "EmailSubmission/queryChanges")]
    QueryChangesEmailSubmission,
    #[serde(rename = "EmailSubmission/set")]
    SetEmailSubmission,
    #[serde(rename = "VacationResponse/get")]
    GetVacationResponse,
    #[serde(rename = "VacationResponse/set")]
    SetVacationResponse,
    #[serde(rename = "error")]
    Error,
}

struct MaybeIdReferenceVisitor;

impl<'de> serde::de::Visitor<'de> for MaybeIdReferenceVisitor {
    type Value = MaybeIdReference;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a valid JMAP state")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Ok(if !v.starts_with('#') {
            MaybeIdReference::Value(JMAPId::parse(v).ok_or_else(|| {
                serde::de::Error::custom(format!("Failed to parse JMAP id '{}'", v))
            })?)
        } else {
            MaybeIdReference::Reference(
                v.get(1..)
                    .ok_or_else(|| {
                        serde::de::Error::custom(format!("Failed to parse JMAP id '{}'", v))
                    })?
                    .to_string(),
            )
        })
    }
}

impl<'de> serde::Deserialize<'de> for MaybeIdReference {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(MaybeIdReferenceVisitor)
    }
}
