use super::{
    conv::IntoForm,
    parse::get_message_part,
    schema::{
        BodyProperty, Email, EmailBodyPart, EmailBodyValue, EmailHeader, HeaderForm,
        HeaderProperty, Property, Value,
    },
    sharing::JMAPShareMail,
    GetRawHeader,
};
use crate::mail::{HeaderName, MessageData, MessageField, MimePart, MimePartType};
use jmap::{
    from_timestamp,
    jmap_store::get::{GetHelper, GetObject},
    orm::serialize::JMAPOrm,
    request::{
        get::{GetRequest, GetResponse},
        ACLEnforce, MaybeIdReference,
    },
    types::{blob::JMAPBlob, jmap::JMAPId},
    SUPERUSER_ID,
};
use mail_parser::{
    parsers::preview::{preview_html, preview_text, truncate_html, truncate_text},
    HeaderValue, Message, RfcHeader,
};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use store::{
    blob::BlobId,
    core::acl::{ACLToken, ACL},
    AccountId, JMAPStore,
};
use store::{
    core::{collection::Collection, error::StoreError},
    serialize::StoreDeserialize,
};
use store::{DocumentId, Store};

enum FetchRaw {
    Header,
    All,
    None,
}

#[derive(Debug, Clone, Default)]
pub struct GetArguments {
    pub body_properties: Option<Vec<BodyProperty>>,
    pub fetch_text_body_values: Option<bool>,
    pub fetch_html_body_values: Option<bool>,
    pub fetch_all_body_values: Option<bool>,
    pub max_body_value_bytes: Option<usize>,
}

impl GetObject for Email {
    type GetArguments = GetArguments;

    fn default_properties() -> Vec<Self::Property> {
        vec![
            Property::Id,
            Property::BlobId,
            Property::ThreadId,
            Property::MailboxIds,
            Property::Keywords,
            Property::Size,
            Property::ReceivedAt,
            Property::MessageId,
            Property::InReplyTo,
            Property::References,
            Property::Sender,
            Property::From,
            Property::To,
            Property::Cc,
            Property::Bcc,
            Property::ReplyTo,
            Property::Subject,
            Property::SentAt,
            Property::HasAttachment,
            Property::Preview,
            Property::BodyValues,
            Property::TextBody,
            Property::HtmlBody,
            Property::Attachments,
        ]
    }

    fn get_as_id(&self, property: &Self::Property) -> Option<Vec<JMAPId>> {
        match self.properties.get(property)? {
            Value::Id { value } => Some(vec![*value]),
            Value::MailboxIds { value, .. } => {
                Some(value.keys().filter_map(|id| Some(*id.value()?)).collect())
            }
            _ => None,
        }
    }
}

impl Email {
    pub fn default_body_properties() -> Vec<BodyProperty> {
        vec![
            BodyProperty::PartId,
            BodyProperty::BlobId,
            BodyProperty::Size,
            BodyProperty::Name,
            BodyProperty::Type,
            BodyProperty::Charset,
            BodyProperty::Disposition,
            BodyProperty::Cid,
            BodyProperty::Language,
            BodyProperty::Location,
        ]
    }
}

pub enum BlobResult {
    Blob(Vec<u8>),
    Unauthorized,
    NotFound,
}

pub trait JMAPGetMail<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_get(&self, request: GetRequest<Email>) -> jmap::Result<GetResponse<Email>>;
    fn mail_blob_get(
        &self,
        account_id: AccountId,
        acl: &Arc<ACLToken>,
        blob: &JMAPBlob,
    ) -> store::Result<BlobResult>;
}

impl<T> JMAPGetMail<T> for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn mail_get(&self, request: GetRequest<Email>) -> jmap::Result<GetResponse<Email>> {
        // Initialize helpers
        let account_id = request.account_id.get_document_id();
        let mut helper = GetHelper::new(
            self,
            request,
            Some(|ids: Vec<DocumentId>| {
                Ok(self
                    .get_multi_document_value(
                        account_id,
                        Collection::Mail,
                        ids.iter().copied(),
                        MessageField::ThreadId.into(),
                    )?
                    .into_iter()
                    .zip(ids)
                    .filter_map(
                        |(thread_id, document_id): (Option<DocumentId>, DocumentId)| {
                            JMAPId::from_parts(thread_id?, document_id).into()
                        },
                    )
                    .collect::<Vec<JMAPId>>())
            }),
            (|account_id: AccountId, member_of: &[AccountId]| {
                self.mail_shared_messages(account_id, member_of, ACL::ReadItems)
            })
            .into(),
        )?;

        // Process arguments
        let body_properties = helper
            .request
            .arguments
            .body_properties
            .take()
            .and_then(|p| if !p.is_empty() { Some(p) } else { None })
            .unwrap_or_else(Email::default_body_properties);
        let fetch_text_body_values = helper
            .request
            .arguments
            .fetch_text_body_values
            .unwrap_or(false);
        let fetch_html_body_values = helper
            .request
            .arguments
            .fetch_html_body_values
            .unwrap_or(false);
        let fetch_all_body_values = helper
            .request
            .arguments
            .fetch_all_body_values
            .unwrap_or(false);
        let max_body_value_bytes = helper.request.arguments.max_body_value_bytes.unwrap_or(0);
        let fetch_raw = if body_properties
            .iter()
            .any(|prop| matches!(prop, BodyProperty::Headers | BodyProperty::Header(_)))
        {
            FetchRaw::All
        } else if helper.properties.iter().any(|prop| {
            matches!(
                prop,
                Property::Header(HeaderProperty {
                    form: HeaderForm::Raw,
                    ..
                }) | Property::Header(HeaderProperty {
                    header: HeaderName::Other(_),
                    ..
                }) | Property::BodyStructure
            )
        }) {
            FetchRaw::Header
        } else {
            FetchRaw::None
        };

        // Get items
        helper.get(|id, properties| {
            let document_id = id.get_document_id();

            // Fetch message metadata
            let message_data_bytes = self
                .blob_get(
                    &self
                        .get_document_value::<BlobId>(
                            account_id,
                            Collection::Mail,
                            document_id,
                            MessageField::Metadata.into(),
                        )?
                        .ok_or_else(|| {
                            StoreError::DataCorruption(format!(
                                "Email metadata blobId for {}/{} does not exist.",
                                account_id, document_id
                            ))
                        })?,
                )?
                .ok_or_else(|| {
                    StoreError::DataCorruption(format!(
                        "Email metadata blob linked to {}/{} does not exist.",
                        account_id, document_id
                    ))
                })?;

            // Deserialize message data
            let mut message_data =
                MessageData::deserialize(&message_data_bytes).ok_or_else(|| {
                    StoreError::DataCorruption(format!(
                        "Failed to deserialize email metadata for {}/{}",
                        account_id, document_id
                    ))
                })?;

            // Fetch raw message only if needed
            let raw_message = match &fetch_raw {
                FetchRaw::All => {
                    Some(self.blob_get(&message_data.raw_message)?.ok_or_else(|| {
                        StoreError::DataCorruption(format!(
                            "Raw email message not found for {}/{}.",
                            account_id, document_id
                        ))
                    })?)
                }
                FetchRaw::Header => Some(
                    self.blob_get_range(
                        &message_data.raw_message,
                        0..message_data.body_offset as u32,
                    )?
                    .ok_or_else(|| {
                        StoreError::DataCorruption(format!(
                            "Raw email message not found for {}/{}.",
                            account_id, document_id
                        ))
                    })?,
                ),
                FetchRaw::None => None,
            };

            // Fetch ORM
            let fields = self
                .get_orm::<Email>(account_id, document_id)?
                .ok_or_else(|| StoreError::InternalError("ORM not found for Email.".to_string()))?;

            // Add requested properties to result
            let mut email = HashMap::with_capacity(properties.len());
            for property in properties {
                let value = match property {
                    Property::Id => Value::Id { value: id }.into(),
                    Property::BlobId => Value::Blob {
                        value: JMAPBlob::from(&message_data.raw_message),
                    }
                    .into(),
                    Property::ThreadId => Value::Id {
                        value: id.get_prefix_id().into(),
                    }
                    .into(),
                    Property::MailboxIds => {
                        fields
                            .get_tags(&Property::MailboxIds)
                            .map(|tags| Value::MailboxIds {
                                value: tags
                                    .iter()
                                    .map(|tag| (MaybeIdReference::Value(tag.as_id().into()), true))
                                    .collect(),
                                set: true,
                            })
                    }
                    Property::Keywords => {
                        fields
                            .get_tags(&Property::Keywords)
                            .map(|tags| Value::Keywords {
                                value: tags.iter().map(|tag| (tag.into(), true)).collect(),
                                set: true,
                            })
                    }
                    Property::Size => Value::Size {
                        value: message_data.size,
                    }
                    .into(),
                    Property::ReceivedAt => Value::Date {
                        value: from_timestamp(message_data.received_at),
                    }
                    .into(),
                    Property::MessageId | Property::InReplyTo | Property::References => {
                        message_data.header(
                            &property.as_rfc_header(),
                            &HeaderForm::MessageIds,
                            false,
                        )
                    }
                    Property::Sender
                    | Property::From
                    | Property::To
                    | Property::Cc
                    | Property::Bcc
                    | Property::ReplyTo => message_data.header(
                        &property.as_rfc_header(),
                        &HeaderForm::Addresses,
                        false,
                    ),
                    Property::Subject => {
                        message_data.header(&RfcHeader::Subject, &HeaderForm::Text, false)
                    }
                    Property::SentAt => {
                        message_data.header(&RfcHeader::Date, &HeaderForm::Date, false)
                    }
                    Property::HasAttachment => Value::Bool {
                        value: message_data.has_attachments,
                    }
                    .into(),
                    Property::Header(header) => {
                        match (&header.header, &header.form, &raw_message) {
                            (
                                header_name @ HeaderName::Other(_),
                                header_form,
                                Some(raw_message),
                            )
                            | (
                                header_name @ HeaderName::Rfc(_),
                                header_form @ HeaderForm::Raw,
                                Some(raw_message),
                            ) => {
                                if let Some(offsets) = message_data
                                    .mime_parts
                                    .get(0)
                                    .and_then(|h| h.raw_headers.get_header(header_name))
                                {
                                    header_form
                                        .parse_offsets(&offsets, raw_message, header.all)
                                        .into_form(header_form, header.all)
                                } else {
                                    None
                                }
                            }
                            (HeaderName::Rfc(header_name), header_form, _) => {
                                message_data.header(header_name, header_form, header.all)
                            }
                            _ => None,
                        }
                    }
                    Property::Preview => {
                        if !message_data.text_body.is_empty() || !message_data.html_body.is_empty()
                        {
                            #[allow(clippy::type_complexity)]
                            let (parts, preview_fnc): (
                                &Vec<usize>,
                                fn(Cow<str>, usize) -> Cow<str>,
                            ) = if !message_data.text_body.is_empty() {
                                (&message_data.text_body, preview_text)
                            } else {
                                (&message_data.html_body, preview_html)
                            };

                            Value::Text {
                                value: preview_fnc(
                                    String::from_utf8(
                                        self.blob_get(
                                            parts
                                                .get(0)
                                                .and_then(|p| message_data.mime_parts.get(*p))
                                                .ok_or_else(|| {
                                                    StoreError::DataCorruption(format!(
                                                        "Missing message part for {}/{}",
                                                        account_id, document_id
                                                    ))
                                                })?
                                                .mime_type
                                                .blob_id()
                                                .ok_or_else(|| {
                                                    StoreError::DataCorruption(format!(
                                                        "Message part blobId not found for {}/{}.",
                                                        account_id, document_id
                                                    ))
                                                })?,
                                        )?
                                        .ok_or_else(
                                            || {
                                                StoreError::DataCorruption(format!(
                                                    "Message part blob not found for {}/{}.",
                                                    account_id, document_id
                                                ))
                                            },
                                        )?,
                                    )
                                    .map_or_else(
                                        |err| String::from_utf8_lossy(err.as_bytes()).into_owned(),
                                        |s| s,
                                    )
                                    .into(),
                                    256,
                                )
                                .into_owned(),
                            }
                            .into()
                        } else {
                            None
                        }
                    }
                    Property::BodyValues => {
                        let mut body_values = HashMap::new();
                        for (part_id, mime_part) in message_data.mime_parts.iter().enumerate() {
                            if ((mime_part.mime_type.is_html()
                                && (fetch_all_body_values || fetch_html_body_values))
                                || (mime_part.mime_type.is_text()
                                    && (fetch_all_body_values || fetch_text_body_values)))
                                && (message_data.text_body.contains(&part_id)
                                    || message_data.html_body.contains(&part_id))
                            {
                                let blob = self
                                    .blob_get(mime_part.mime_type.blob_id().ok_or_else(|| {
                                        StoreError::DataCorruption(format!(
                                            "BodyValue blobId not found for {}/{}.",
                                            account_id, document_id
                                        ))
                                    })?)?
                                    .ok_or_else(|| {
                                        StoreError::DataCorruption(format!(
                                            "BodyValue blob not found for {}/{}.",
                                            account_id, document_id
                                        ))
                                    })?;

                                body_values.insert(
                                    part_id.to_string(),
                                    mime_part.as_body_value(
                                        String::from_utf8(blob).map_or_else(
                                            |err| {
                                                String::from_utf8_lossy(err.as_bytes()).into_owned()
                                            },
                                            |s| s,
                                        ),
                                        max_body_value_bytes,
                                    ),
                                );
                            }
                        }
                        if !body_values.is_empty() {
                            Value::BodyValues { value: body_values }.into()
                        } else {
                            None
                        }
                    }
                    Property::TextBody => Some(
                        message_data
                            .mime_parts
                            .as_body_parts(
                                &message_data.text_body,
                                &body_properties,
                                raw_message.as_deref(),
                                None,
                            )
                            .into(),
                    ),
                    Property::HtmlBody => Some(
                        message_data
                            .mime_parts
                            .as_body_parts(
                                &message_data.html_body,
                                &body_properties,
                                raw_message.as_deref(),
                                None,
                            )
                            .into(),
                    ),
                    Property::Attachments => Some(
                        message_data
                            .mime_parts
                            .as_body_parts(
                                &message_data.attachments,
                                &body_properties,
                                raw_message.as_deref(),
                                None,
                            )
                            .into(),
                    ),
                    Property::BodyStructure => message_data
                        .mime_parts
                        .as_body_structure(&body_properties, raw_message.as_deref(), None)
                        .map(|b| b.into()),
                    Property::Invalid(_) => None,
                };

                email.insert(property.clone(), value.unwrap_or_default());
            }

            Ok(Some(Email { properties: email }))
        })
    }

    fn mail_blob_get(
        &self,
        account_id: AccountId,
        acl: &Arc<ACLToken>,
        blob: &JMAPBlob,
    ) -> store::Result<BlobResult> {
        if !self.blob_account_has_access(&blob.id, &acl.member_of)? && !acl.is_member(SUPERUSER_ID)
        {
            if let Some(shared_ids) = self
                .mail_shared_messages(account_id, &acl.member_of, ACL::ReadItems)?
                .as_ref()
            {
                if !self.blob_document_has_access(
                    &blob.id,
                    account_id,
                    Collection::Mail,
                    shared_ids,
                )? {
                    return Ok(BlobResult::Unauthorized);
                }
            } else {
                return Ok(BlobResult::Unauthorized);
            }
        }

        let bytes = self.blob_get(&blob.id)?;
        Ok(if let (Some(message), Some(inner_id)) = (
            bytes.as_ref().and_then(|b| Message::parse(b)),
            blob.inner_id,
        ) {
            get_message_part(message, inner_id, false).map(|bytes| bytes.into_owned())
        } else {
            bytes
        }
        .map(BlobResult::Blob)
        .unwrap_or(BlobResult::NotFound))
    }
}

impl MimePart {
    pub fn as_body_part(
        &self,
        part_id: usize,
        properties: &[BodyProperty],
        message_raw: Option<&[u8]>,
        base_blob_id: Option<&BlobId>,
    ) -> EmailBodyPart {
        let mut body_part = HashMap::with_capacity(properties.len());
        let blob_id = self.mime_type.blob_id();

        for property in properties {
            match property {
                BodyProperty::PartId if blob_id.is_some() => {
                    body_part.insert(
                        BodyProperty::PartId,
                        Value::Text {
                            value: part_id.to_string(),
                        },
                    );
                }
                BodyProperty::BlobId if blob_id.is_some() => {
                    body_part.insert(
                        BodyProperty::BlobId,
                        Value::Blob {
                            value: if let Some(base_blob_id) = base_blob_id {
                                JMAPBlob::new_inner(base_blob_id.clone(), part_id as u32)
                            } else {
                                JMAPBlob::from(*blob_id.as_ref().unwrap())
                            },
                        },
                    );
                }
                BodyProperty::Size if blob_id.is_some() => {
                    body_part.insert(BodyProperty::Size, Value::Size { value: self.size });
                }
                BodyProperty::Name => {
                    if let Some(name) = &self.name {
                        body_part.insert(
                            BodyProperty::Name,
                            Value::Text {
                                value: name.to_string(),
                            },
                        );
                    }
                }
                BodyProperty::Type => {
                    if let Some(mime_type) = &self.type_ {
                        body_part.insert(
                            BodyProperty::Type,
                            Value::Text {
                                value: mime_type.to_string(),
                            },
                        );
                    }
                }
                BodyProperty::Charset => {
                    if let Some(charset) = &self.charset {
                        body_part.insert(
                            BodyProperty::Charset,
                            Value::Text {
                                value: charset.to_string(),
                            },
                        );
                    }
                }
                BodyProperty::Disposition => {
                    if let Some(disposition) = &self.disposition {
                        body_part.insert(
                            BodyProperty::Disposition,
                            Value::Text {
                                value: disposition.to_string(),
                            },
                        );
                    }
                }
                BodyProperty::Cid => {
                    if let Some(cid) = &self.cid {
                        body_part.insert(
                            BodyProperty::Cid,
                            Value::Text {
                                value: cid.to_string(),
                            },
                        );
                    }
                }
                BodyProperty::Language => {
                    if let Some(language) = &self.language {
                        body_part.insert(
                            BodyProperty::Language,
                            Value::TextList {
                                value: language.to_vec(),
                            },
                        );
                    }
                }
                BodyProperty::Location => {
                    if let Some(location) = &self.location {
                        body_part.insert(
                            BodyProperty::Location,
                            Value::Text {
                                value: location.to_string(),
                            },
                        );
                    }
                }
                BodyProperty::Header(header) if message_raw.is_some() => {
                    if let Some(offsets) = self.raw_headers.get_header(&header.header) {
                        if let Some(value) = header
                            .form
                            .parse_offsets(&offsets, message_raw.unwrap(), header.all)
                            .into_form(&header.form, header.all)
                        {
                            body_part.insert(BodyProperty::Header(header.clone()), value);
                        }
                    }
                }
                BodyProperty::Headers if message_raw.is_some() && !self.raw_headers.is_empty() => {
                    let raw_message = message_raw.unwrap();
                    let mut headers = Vec::with_capacity(self.raw_headers.len());
                    for (header, value) in &self.raw_headers {
                        if let HeaderValue::Collection(values) =
                            HeaderForm::Raw.parse_offsets(&[value], raw_message, true)
                        {
                            for value in values {
                                if let HeaderValue::Text(value) = value {
                                    headers.push(EmailHeader {
                                        name: header.as_str().to_string(),
                                        value: value.into_owned(),
                                    });
                                }
                            }
                        }
                    }
                    body_part.insert(BodyProperty::Headers, Value::Headers { value: headers });
                }
                _ => (),
            }
        }

        EmailBodyPart {
            properties: body_part,
        }
    }

    pub fn as_body_value(&self, body_value: String, max_body_value: usize) -> EmailBodyValue {
        EmailBodyValue {
            is_encoding_problem: self.is_encoding_problem.into(),
            is_truncated: (max_body_value > 0 && body_value.len() > max_body_value).into(),
            value: if max_body_value == 0 || body_value.len() <= max_body_value {
                body_value
            } else if matches!(&self.mime_type, MimePartType::Html { .. }) {
                truncate_html(body_value.into(), max_body_value).to_string()
            } else {
                truncate_text(body_value.into(), max_body_value).to_string()
            },
        }
    }
}

pub trait AsBodyParts {
    fn as_body_parts(
        &self,
        parts: &[usize],
        properties: &[BodyProperty],
        message_raw: Option<&[u8]>,
        base_blob_id: Option<&BlobId>,
    ) -> Vec<EmailBodyPart>;
}

impl AsBodyParts for Vec<MimePart> {
    fn as_body_parts(
        &self,
        parts: &[usize],
        properties: &[BodyProperty],
        message_raw: Option<&[u8]>,
        base_blob_id: Option<&BlobId>,
    ) -> Vec<EmailBodyPart> {
        parts
            .iter()
            .filter_map(|part_id| {
                Some(self.get(*part_id)?.as_body_part(
                    *part_id,
                    properties,
                    message_raw,
                    base_blob_id,
                ))
            })
            .collect::<Vec<_>>()
    }
}

pub trait AsBodyStructure {
    fn as_body_structure(
        &self,
        properties: &[BodyProperty],
        message_raw: Option<&[u8]>,
        base_blob_id: Option<&BlobId>,
    ) -> Option<EmailBodyPart>;
}

impl AsBodyStructure for Vec<MimePart> {
    fn as_body_structure(
        &self,
        properties: &[BodyProperty],
        message_raw: Option<&[u8]>,
        base_blob_id: Option<&BlobId>,
    ) -> Option<EmailBodyPart> {
        let mut stack = Vec::new();
        let root_part = self.get(0)?;
        let mut body_structure = root_part.as_body_part(0, properties, message_raw, base_blob_id);

        if let MimePartType::MultiPart {
            subparts: part_list,
        } = &root_part.mime_type
        {
            let mut subparts = Vec::with_capacity(part_list.len());
            let mut part_list_iter = part_list.iter();

            loop {
                while let Some(part_id) = part_list_iter.next() {
                    let subpart = self.get(*part_id)?;

                    subparts.push(self.get(*part_id)?.as_body_part(
                        *part_id,
                        properties,
                        message_raw,
                        base_blob_id,
                    ));

                    if let MimePartType::MultiPart {
                        subparts: part_list,
                    } = &subpart.mime_type
                    {
                        stack.push((part_list_iter, subparts));
                        part_list_iter = part_list.iter();
                        subparts = Vec::with_capacity(part_list.len());
                    }
                }

                if let Some((prev_part_list_iter, mut prev_subparts)) = stack.pop() {
                    let prev_part = prev_subparts.last_mut().unwrap();
                    prev_part.properties.insert(
                        BodyProperty::Subparts,
                        Value::BodyPartList { value: subparts },
                    );
                    part_list_iter = prev_part_list_iter;
                    subparts = prev_subparts;
                } else {
                    break;
                }
            }

            body_structure.properties.insert(
                BodyProperty::Subparts,
                Value::BodyPartList { value: subparts },
            );
        }

        body_structure.into()
    }
}
