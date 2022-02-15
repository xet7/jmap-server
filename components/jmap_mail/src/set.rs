use chrono::DateTime;
use jmap_store::blob::JMAPLocalBlobStore;
use jmap_store::changes::JMAPLocalChanges;
use jmap_store::id::{BlobId, JMAPIdSerialize};
use jmap_store::json::JSONValue;
use jmap_store::local_store::JMAPLocalStore;
use jmap_store::{
    json::JSONPointer, JMAPError, JMAPId, JMAPSet, JMAPSetErrorType, JMAPSetResponse, JMAP_MAIL,
    JMAP_MAILBOX,
};
use mail_builder::headers::address::Address;
use mail_builder::headers::content_type::ContentType;
use mail_builder::headers::date::Date;
use mail_builder::headers::message_id::MessageId;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::text::Text;
use mail_builder::headers::url::URL;
use mail_builder::mime::{BodyPart, MimePart};
use mail_builder::MessageBuilder;
use mail_parser::HeaderName;
use std::collections::{BTreeMap, HashMap};
use store::field::FieldOptions;
use store::{
    batch::{DocumentWriter, LogAction},
    DocumentSet,
};
use store::{AccountId, DocumentId, Store, Tag};

use crate::import::{bincode_deserialize, bincode_serialize, JMAPMailLocalStoreImport};
use crate::parse::get_message_blob;
use crate::query::MailboxId;
use crate::{
    JMAPMailHeaderForm, JMAPMailHeaderProperty, JMAPMailIdImpl, JMAPMailProperties, JMAPMailSet,
    MessageField,
};

impl<'x, T> JMAPMailSet<'x> for JMAPLocalStore<T>
where
    T: Store<'x>,
{
    fn mail_set(&'x self, request: JMAPSet) -> jmap_store::Result<JMAPSetResponse> {
        let old_state = self.get_state(request.account_id, JMAP_MAIL)?;
        if let Some(if_in_state) = request.if_in_state {
            if old_state != if_in_state {
                return Err(JMAPError::StateMismatch);
            }
        }

        let total_changes = request.create.to_object().map_or(0, |c| c.len())
            + request.update.to_object().map_or(0, |c| c.len())
            + request.destroy.to_array().map_or(0, |c| c.len());
        if total_changes > self.mail_config.set_max_changes {
            return Err(JMAPError::RequestTooLarge);
        }

        let mut changes = Vec::with_capacity(total_changes);
        let mut response = JMAPSetResponse {
            old_state,
            ..Default::default()
        };
        let document_ids = self.store.get_document_ids(request.account_id, JMAP_MAIL)?;
        let mut mailbox_ids = None;

        if let JSONValue::Object(create) = request.create {
            let mut created = HashMap::with_capacity(create.len());
            let mut not_created = HashMap::with_capacity(create.len());

            for (create_id, message_fields) in create {
                let mailbox_ids = if let Some(mailbox_ids) = &mailbox_ids {
                    mailbox_ids
                } else {
                    mailbox_ids = self
                        .store
                        .get_document_ids(request.account_id, JMAP_MAILBOX)?
                        .into();
                    mailbox_ids.as_ref().unwrap()
                };

                match build_message(self, request.account_id, message_fields, mailbox_ids) {
                    Ok(import_item) => {
                        created.insert(
                            create_id,
                            self.mail_import_blob(
                                request.account_id,
                                &import_item.blob,
                                import_item.mailbox_ids,
                                import_item.keywords,
                                import_item.received_at,
                            )?,
                        );
                    }
                    Err(err) => {
                        not_created.insert(create_id, err);
                    }
                }
            }

            if !created.is_empty() {
                response.created = created.into();
            }

            if !not_created.is_empty() {
                response.not_created = not_created.into();
            }
        }

        if let JSONValue::Object(update) = request.update {
            let mut updated = HashMap::with_capacity(update.len());
            let mut not_updated = HashMap::with_capacity(update.len());

            'main: for (jmap_id_str, properties) in update {
                let (jmap_id, properties) = if let (Some(jmap_id), Some(properties)) = (
                    JMAPId::from_jmap_string(&jmap_id_str),
                    properties.unwrap_object(),
                ) {
                    (jmap_id, properties)
                } else {
                    not_updated.insert(
                        jmap_id_str,
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            "Failed to parse request.",
                        ),
                    );
                    continue 'main;
                };
                let document_id = jmap_id.get_document_id();
                if !document_ids.contains(document_id) {
                    not_updated.insert(
                        jmap_id_str,
                        JSONValue::new_error(JMAPSetErrorType::NotFound, "ID not found."),
                    );
                    continue;
                } else if let JSONValue::Array(destroy_ids) = &request.destroy {
                    if destroy_ids
                        .iter()
                        .any(|x| x.to_string().map(|v| v == jmap_id_str).unwrap_or(false))
                    {
                        not_updated.insert(
                            jmap_id_str,
                            JSONValue::new_error(
                                JMAPSetErrorType::WillDestroy,
                                "ID will be destroyed.",
                            ),
                        );
                        continue;
                    }
                }
                let mut document = DocumentWriter::update(JMAP_MAIL, document_id);

                let mut keyword_op_list = HashMap::new();
                let mut keyword_op_clear_all = false;
                let mut mailbox_op_list = HashMap::new();
                let mut mailbox_op_clear_all = false;

                for (field, value) in properties {
                    match JSONPointer::parse(&field).unwrap_or(JSONPointer::Root) {
                        JSONPointer::String(field) => {
                            match JMAPMailProperties::parse(&field)
                                .unwrap_or(JMAPMailProperties::Id)
                            {
                                JMAPMailProperties::Keywords => {
                                    if let JSONValue::Object(value) = value {
                                        // Add keywords to the list
                                        for (keyword, value) in value {
                                            if let JSONValue::Bool(true) = value {
                                                keyword_op_list
                                                    .insert(Tag::Text(keyword.into()), true);
                                            }
                                        }
                                        keyword_op_clear_all = true;
                                    } else {
                                        not_updated.insert(
                                            jmap_id_str,
                                            JSONValue::new_invalid_property(
                                                "keywords",
                                                "Expected an object.",
                                            ),
                                        );
                                        continue 'main;
                                    }
                                }
                                JMAPMailProperties::MailboxIds => {
                                    // Unwrap JSON object
                                    if let JSONValue::Object(value) = value {
                                        // Add mailbox ids to the list
                                        for (mailbox_id, value) in value {
                                            match (
                                                JMAPId::from_jmap_string(mailbox_id.as_ref()),
                                                value,
                                            ) {
                                                (Some(mailbox_id), JSONValue::Bool(true)) => {
                                                    mailbox_op_list
                                                        .insert(mailbox_id.get_document_id(), true);
                                                }
                                                (None, _) => {
                                                    not_updated.insert(
                                                        jmap_id_str,
                                                        JSONValue::new_invalid_property(
                                                            format!("mailboxIds/{}", mailbox_id),
                                                            "Failed to parse mailbox id.",
                                                        ),
                                                    );
                                                    continue 'main;
                                                }
                                                _ => (),
                                            }
                                        }
                                        mailbox_op_clear_all = true;
                                    } else {
                                        // mailboxIds is not a JSON object
                                        not_updated.insert(
                                            jmap_id_str,
                                            JSONValue::new_invalid_property(
                                                "mailboxIds",
                                                "Expected an object.",
                                            ),
                                        );
                                        continue 'main;
                                    }
                                }
                                _ => {
                                    not_updated.insert(
                                        jmap_id_str,
                                        JSONValue::new_invalid_property(
                                            field,
                                            "Unsupported property.",
                                        ),
                                    );
                                    continue 'main;
                                }
                            }
                        }

                        JSONPointer::Path(mut path) if path.len() == 2 => {
                            if let (JSONPointer::String(property), JSONPointer::String(field)) =
                                (path.pop().unwrap(), path.pop().unwrap())
                            {
                                let is_mailbox = match JMAPMailProperties::parse(&field)
                                    .unwrap_or(JMAPMailProperties::Id)
                                {
                                    JMAPMailProperties::MailboxIds => true,
                                    JMAPMailProperties::Keywords => false,
                                    _ => {
                                        not_updated.insert(
                                            jmap_id_str,
                                            JSONValue::new_invalid_property(
                                                format!("{}/{}", field, property),
                                                "Unsupported property.",
                                            ),
                                        );
                                        continue 'main;
                                    }
                                };
                                match value {
                                    JSONValue::Null | JSONValue::Bool(false) => {
                                        if is_mailbox {
                                            if let Some(mailbox_id) =
                                                JMAPId::from_jmap_string(property.as_ref())
                                            {
                                                mailbox_op_list
                                                    .insert(mailbox_id.get_document_id(), false);
                                            }
                                        } else {
                                            keyword_op_list
                                                .insert(Tag::Text(property.into()), false);
                                        }
                                    }
                                    JSONValue::Bool(true) => {
                                        if is_mailbox {
                                            if let Some(mailbox_id) =
                                                JMAPId::from_jmap_string(property.as_ref())
                                            {
                                                mailbox_op_list
                                                    .insert(mailbox_id.get_document_id(), true);
                                            }
                                        } else {
                                            keyword_op_list
                                                .insert(Tag::Text(property.into()), true);
                                        }
                                    }
                                    _ => {
                                        not_updated.insert(
                                            jmap_id_str,
                                            JSONValue::new_invalid_property(
                                                format!("{}/{}", field, property),
                                                "Expected a boolean or null value.",
                                            ),
                                        );
                                        continue 'main;
                                    }
                                }
                            } else {
                                not_updated.insert(
                                    jmap_id_str,
                                    JSONValue::new_invalid_property(field, "Unsupported property."),
                                );
                                continue 'main;
                            }
                        }
                        _ => {
                            not_updated.insert(
                                jmap_id_str,
                                JSONValue::new_invalid_property(
                                    field.to_string(),
                                    "Unsupported property.",
                                ),
                            );
                            continue 'main;
                        }
                    }
                }

                if !mailbox_op_list.is_empty() || mailbox_op_clear_all {
                    // Obtain mailboxes
                    let mailbox_ids = if let Some(mailbox_ids) = &mailbox_ids {
                        mailbox_ids
                    } else {
                        mailbox_ids = self
                            .store
                            .get_document_ids(request.account_id, JMAP_MAILBOX)?
                            .into();
                        mailbox_ids.as_ref().unwrap()
                    };

                    // Deserialize mailbox list
                    let current_mailboxes = if let Some(current_mailboxes) =
                        self.store.get_document_value::<Vec<u8>>(
                            request.account_id,
                            JMAP_MAIL,
                            document_id,
                            MessageField::Mailbox.into(),
                        )? {
                        bincode_deserialize::<Vec<MailboxId>>(&current_mailboxes)?
                    } else {
                        vec![]
                    };

                    let mut new_mailboxes = Vec::with_capacity(std::cmp::max(
                        mailbox_op_list.len(),
                        current_mailboxes.len(),
                    ));

                    for mailbox_id in &current_mailboxes {
                        if mailbox_op_clear_all {
                            // Untag mailbox unless it is in the list of mailboxes to tag
                            if !mailbox_op_list.get(mailbox_id).unwrap_or(&false) {
                                document.tag(
                                    MessageField::Mailbox,
                                    Tag::Id(*mailbox_id),
                                    FieldOptions::Clear,
                                );
                            }
                        } else if !mailbox_op_list.get(mailbox_id).unwrap_or(&true) {
                            // Untag mailbox if is marked for untagging
                            document.tag(
                                MessageField::Mailbox,
                                Tag::Id(*mailbox_id),
                                FieldOptions::Clear,
                            );
                        } else {
                            // Keep mailbox in the list
                            new_mailboxes.push(*mailbox_id);
                        }
                    }

                    for (mailbox_id, do_create) in mailbox_op_list {
                        if do_create {
                            // Make sure the mailbox exists
                            if mailbox_ids.contains(mailbox_id) {
                                // Tag mailbox if it is not already tagged
                                if !current_mailboxes.contains(&mailbox_id) {
                                    document.tag(
                                        MessageField::Mailbox,
                                        Tag::Id(mailbox_id),
                                        FieldOptions::None,
                                    );
                                }
                                new_mailboxes.push(mailbox_id);
                            } else {
                                not_updated.insert(
                                    jmap_id_str,
                                    JSONValue::new_invalid_property(
                                        format!("mailboxIds/{}", mailbox_id),
                                        "Mailbox does not exist.",
                                    ),
                                );
                                continue 'main;
                            }
                        }
                    }

                    // Messages have to be in at least one mailbox
                    if new_mailboxes.is_empty() {
                        not_updated.insert(
                            jmap_id_str,
                            JSONValue::new_invalid_property(
                                "mailboxIds",
                                "Message must belong to at least one mailbox.",
                            ),
                        );
                        continue 'main;
                    }

                    // Serialize new mailbox list
                    document.binary(
                        MessageField::Mailbox,
                        bincode_serialize(&new_mailboxes)?.into(),
                        FieldOptions::Store,
                    );
                }

                if !keyword_op_list.is_empty() || keyword_op_clear_all {
                    // Deserialize current keywords
                    let current_keywords = if let Some(current_keywords) =
                        self.store.get_document_value::<Vec<u8>>(
                            request.account_id,
                            JMAP_MAIL,
                            document_id,
                            MessageField::Keyword.into(),
                        )? {
                        bincode_deserialize::<Vec<Tag>>(&current_keywords)?
                    } else {
                        vec![]
                    };

                    let mut new_keywords = Vec::with_capacity(std::cmp::max(
                        keyword_op_list.len(),
                        current_keywords.len(),
                    ));

                    for keyword in &current_keywords {
                        if keyword_op_clear_all {
                            // Untag keyword unless it is in the list of keywords to tag
                            if !keyword_op_list.get(keyword).unwrap_or(&false) {
                                document.tag(
                                    MessageField::Keyword,
                                    keyword.clone(),
                                    FieldOptions::Clear,
                                );
                            }
                        } else if !keyword_op_list.get(keyword).unwrap_or(&true) {
                            // Untag keyword if is marked for untagging
                            document.tag(
                                MessageField::Keyword,
                                keyword.clone(),
                                FieldOptions::Clear,
                            );
                        } else {
                            // Keep keyword in the list
                            new_keywords.push(keyword.clone());
                        }
                    }

                    for (keyword, do_create) in keyword_op_list {
                        if do_create {
                            // Tag keyword if it is not already tagged
                            if !current_keywords.contains(&keyword) {
                                document.tag(
                                    MessageField::Keyword,
                                    keyword.clone(),
                                    FieldOptions::None,
                                );
                            }
                            new_keywords.push(keyword);
                        }
                    }

                    // Serialize new keywords list
                    document.binary(
                        MessageField::Keyword,
                        bincode_serialize(&new_keywords)?.into(),
                        FieldOptions::Store,
                    );
                }

                if !document.is_empty() {
                    document.log_update(jmap_id);
                    changes.push(document);
                    updated.insert(jmap_id_str, JSONValue::Null);
                } else {
                    not_updated.insert(
                        jmap_id_str,
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidPatch,
                            "No changes found in request.",
                        ),
                    );
                }
            }

            if !updated.is_empty() {
                response.updated = updated.into();
            }
            if !not_updated.is_empty() {
                response.not_updated = not_updated.into();
            }
        }

        if let JSONValue::Array(destroy_ids) = request.destroy {
            let mut destroyed = Vec::with_capacity(destroy_ids.len());
            let mut not_destroyed = HashMap::with_capacity(destroy_ids.len());

            for destroy_id in destroy_ids {
                if let Some(jmap_id) = destroy_id.to_jmap_id() {
                    let document_id = jmap_id.get_document_id();
                    if document_ids.contains(document_id) {
                        changes.push(
                            DocumentWriter::delete(JMAP_MAIL, document_id)
                                .log(LogAction::Delete(jmap_id)),
                        );
                        destroyed.push(destroy_id);
                        continue;
                    }
                }
                if let JSONValue::String(destroy_id) = destroy_id {
                    not_destroyed.insert(
                        destroy_id,
                        JSONValue::new_error(JMAPSetErrorType::NotFound, "ID not found."),
                    );
                }
            }

            if !destroyed.is_empty() {
                response.destroyed = destroyed.into();
            }

            if !not_destroyed.is_empty() {
                response.not_destroyed = not_destroyed.into();
            }
        }

        if !changes.is_empty() {
            self.store.update_documents(request.account_id, changes)?;
            response.new_state = self.get_state(request.account_id, JMAP_MAIL)?;
        } else {
            response.new_state = response.old_state.clone();
        }

        Ok(response)
    }
}

struct MessageItem<'x> {
    pub blob: Vec<u8>,
    pub mailbox_ids: Vec<MailboxId>,
    pub keywords: Vec<Tag<'x>>,
    pub received_at: Option<i64>,
}

#[allow(clippy::blocks_in_if_conditions)]
fn build_message<'x, 'y>(
    store: &impl JMAPLocalBlobStore<'y>,
    account: AccountId,
    fields: JSONValue,
    existing_mailboxes: &impl DocumentSet<Item = DocumentId>,
) -> Result<MessageItem<'x>, JSONValue> {
    let fields = if let JSONValue::Object(fields) = fields {
        fields
    } else {
        return Err(JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Failed to parse request.",
        ));
    };

    let body_values = fields.get("bodyValues").and_then(|v| v.to_object());

    let mut builder = MessageBuilder::new();
    let mut mailbox_ids: Vec<MailboxId> = Vec::new();
    let mut keywords: Vec<Tag> = Vec::new();
    let mut received_at: Option<i64> = None;

    for (property, value) in &fields {
        match JMAPMailProperties::parse(property).ok_or_else(|| {
            JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                format!("Failed to parse {}", property),
            )
        })? {
            JMAPMailProperties::MailboxIds => {
                for (mailbox, value) in value.to_object().ok_or_else(|| {
                    JSONValue::new_error(
                        JMAPSetErrorType::InvalidProperties,
                        "Expected object containing mailboxIds",
                    )
                })? {
                    let mailbox_id = JMAPId::from_jmap_string(mailbox)
                        .ok_or_else(|| {
                            JSONValue::new_error(
                                JMAPSetErrorType::InvalidProperties,
                                format!("Failed to parse mailboxId: {}", mailbox),
                            )
                        })?
                        .get_document_id();

                    if value.to_bool().ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            "Expected boolean value in mailboxIds",
                        )
                    })? {
                        if !existing_mailboxes.contains(mailbox_id) {
                            return Err(JSONValue::new_error(
                                JMAPSetErrorType::InvalidProperties,
                                format!("mailboxId {} does not exist.", mailbox),
                            ));
                        }
                        mailbox_ids.push(mailbox_id);
                    }
                }
            }
            JMAPMailProperties::Keywords => {
                for (keyword, value) in value.to_object().ok_or_else(|| {
                    JSONValue::new_error(
                        JMAPSetErrorType::InvalidProperties,
                        "Expected object containing keywords",
                    )
                })? {
                    if value.to_bool().ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            "Expected boolean value in keywords",
                        )
                    })? {
                        keywords.push(Tag::Text(keyword.to_string().into()));
                    }
                }
            }
            JMAPMailProperties::ReceivedAt => {
                received_at = import_json_date(value)?.into();
            }
            JMAPMailProperties::MessageId => builder.header(
                "Message-ID",
                MessageId::from(import_json_string_list(value)?),
            ),
            JMAPMailProperties::InReplyTo => builder.header(
                "In-Reply-To",
                MessageId::from(import_json_string_list(value)?),
            ),
            JMAPMailProperties::References => builder.header(
                "References",
                MessageId::from(import_json_string_list(value)?),
            ),
            JMAPMailProperties::Sender => {
                builder.header("Sender", Address::List(import_json_addresses(value)?))
            }
            JMAPMailProperties::From => {
                builder.header("From", Address::List(import_json_addresses(value)?))
            }
            JMAPMailProperties::To => {
                builder.header("To", Address::List(import_json_addresses(value)?))
            }
            JMAPMailProperties::Cc => {
                builder.header("Cc", Address::List(import_json_addresses(value)?))
            }
            JMAPMailProperties::Bcc => {
                builder.header("Bcc", Address::List(import_json_addresses(value)?))
            }
            JMAPMailProperties::ReplyTo => {
                builder.header("Reply-To", Address::List(import_json_addresses(value)?))
            }
            JMAPMailProperties::Subject => {
                builder.header("Subject", Text::new(import_json_string(value)?));
            }
            JMAPMailProperties::SentAt => {
                builder.header("Date", Date::new(import_json_date(value)?))
            }
            JMAPMailProperties::TextBody => {
                builder.text_body = import_body_parts(
                    store,
                    account,
                    value,
                    body_values,
                    "text/plain".into(),
                    true,
                )?
                .pop()
                .ok_or_else(|| {
                    JSONValue::new_error(
                        JMAPSetErrorType::InvalidProperties,
                        "No text body part found".to_string(),
                    )
                })?
                .into();
            }
            JMAPMailProperties::HtmlBody => {
                builder.html_body = import_body_parts(
                    store,
                    account,
                    value,
                    body_values,
                    "text/html".into(),
                    true,
                )?
                .pop()
                .ok_or_else(|| {
                    JSONValue::new_error(
                        JMAPSetErrorType::InvalidProperties,
                        "No html body part found".to_string(),
                    )
                })?
                .into();
            }
            JMAPMailProperties::Attachments => {
                builder.attachments =
                    import_body_parts(store, account, value, body_values, None, false)?.into();
            }
            JMAPMailProperties::BodyStructure => {
                builder.body = import_body_structure(store, account, value, body_values)?.into();
            }
            JMAPMailProperties::Header(JMAPMailHeaderProperty { form, header, all }) => {
                if !all {
                    import_header(&mut builder, header, form, value)?;
                } else {
                    for value in value.to_array().ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            "Expected an array.".to_string(),
                        )
                    })? {
                        import_header(&mut builder, header.clone(), form.clone(), value)?;
                    }
                }
            }

            JMAPMailProperties::Id
            | JMAPMailProperties::Size
            | JMAPMailProperties::BlobId
            | JMAPMailProperties::Preview
            | JMAPMailProperties::ThreadId
            | JMAPMailProperties::BodyValues
            | JMAPMailProperties::HasAttachment => (),
        }
    }

    if mailbox_ids.is_empty() {
        return Err(JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Message has to belong to at least one mailbox.",
        ));
    }

    if builder.headers.is_empty()
        && builder.body.is_none()
        && builder.html_body.is_none()
        && builder.text_body.is_none()
        && builder.attachments.is_none()
    {
        return Err(JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Message has to have at least one header or body part.",
        ));
    }

    // TODO: write parsed message directly to store, avoid parsing it again.
    let mut blob = Vec::with_capacity(1024);
    builder
        .write_to(&mut blob)
        .map_err(|_| JSONValue::new_error(JMAPSetErrorType::InvalidProperties, "Internal error"))?;

    Ok(MessageItem {
        blob,
        mailbox_ids,
        keywords,
        received_at,
    })
}

fn import_body_structure<'x, 'y>(
    store: &impl JMAPLocalBlobStore<'y>,
    account: AccountId,
    part: &'x JSONValue,
    body_values: Option<&'x HashMap<String, JSONValue>>,
) -> Result<MimePart<'x>, JSONValue> {
    let (mut mime_part, sub_parts) =
        import_body_part(store, account, part, body_values, None, false)?;

    if let Some(sub_parts) = sub_parts {
        let mut stack = Vec::new();
        let mut it = sub_parts.iter();

        loop {
            while let Some(part) = it.next() {
                let (sub_mime_part, sub_parts) =
                    import_body_part(store, account, part, body_values, None, false)?;
                if let Some(sub_parts) = sub_parts {
                    stack.push((mime_part, it));
                    mime_part = sub_mime_part;
                    it = sub_parts.iter();
                } else {
                    mime_part.add_part(sub_mime_part);
                }
            }
            if let Some((mut prev_mime_part, prev_it)) = stack.pop() {
                prev_mime_part.add_part(mime_part);
                mime_part = prev_mime_part;
                it = prev_it;
            } else {
                break;
            }
        }
    }

    Ok(mime_part)
}

fn import_body_part<'x, 'y>(
    store: &impl JMAPLocalBlobStore<'y>,
    account: AccountId,
    part: &'x JSONValue,
    body_values: Option<&'x HashMap<String, JSONValue>>,
    implicit_type: Option<&'x str>,
    strict_implicit_type: bool,
) -> Result<(MimePart<'x>, Option<&'x Vec<JSONValue>>), JSONValue> {
    let part = part.to_object().ok_or_else(|| {
        JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Expected an object in body part list.".to_string(),
        )
    })?;

    let content_type = part
        .get("type")
        .and_then(|v| v.to_string())
        .unwrap_or_else(|| implicit_type.unwrap_or("text/plain"));

    if strict_implicit_type && implicit_type.unwrap() != content_type {
        return Err(JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            format!(
                "Expected exactly body part of type \"{}\"",
                implicit_type.unwrap()
            ),
        ));
    }

    let is_multipart = content_type.starts_with("multipart/");
    let mut mime_part = MimePart {
        headers: BTreeMap::new(),
        contents: if is_multipart {
            BodyPart::Multipart(vec![])
        } else if let Some(part_id) = part.get("partId").and_then(|v| v.to_string()) {
            BodyPart::Text( body_values
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            "Missing \"bodyValues\" object containing partId.".to_string(),
                        )
                    })?
                    .get(part_id)
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            format!("Missing body value for partId \"{}\"", part_id),
                        )
                    })?
                    .to_object()
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            format!("Expected a bodyValues object defining partId \"{}\"", part_id),
                        )
                    })?
                    .get("value")
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            format!("Missing \"value\" field in bodyValues object defining partId \"{}\"", part_id),
                        )
                    })?
                    .to_string()
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            format!("Expected a string \"value\" field in bodyValues object defining partId \"{}\"", part_id),
                        )
                    })?.into())
        } else if let Some(blob_id) = part.get("blobId").and_then(|v| v.to_string()) {
            BodyPart::Binary(
                store
                    .download_blob(
                        account,
                        &BlobId::from_jmap_string(blob_id).ok_or_else(|| {
                            JSONValue::new_error(
                                JMAPSetErrorType::BlobNotFound,
                                "Failed to parse blobId",
                            )
                        })?,
                        get_message_blob,
                    )
                    .map_err(|_| {
                        JSONValue::new_error(
                            JMAPSetErrorType::BlobNotFound,
                            "Failed to fetch blob.",
                        )
                    })?
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::BlobNotFound,
                            "blobId does not exist on this server.",
                        )
                    })?
                    .into(),
            )
        } else {
            return Err(JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                "Expected a \"partId\" or \"blobId\" field in body part.".to_string(),
            ));
        },
    };

    let mut content_type = ContentType::new(content_type);
    if !is_multipart {
        if content_type.c_type.starts_with("text/") {
            if matches!(mime_part.contents, BodyPart::Text(_)) {
                content_type
                    .attributes
                    .insert("charset".into(), "utf-8".into());
            } else if let Some(charset) = part.get("charset") {
                content_type.attributes.insert(
                    "charset".into(),
                    charset
                        .to_string()
                        .ok_or_else(|| {
                            JSONValue::new_error(
                                JMAPSetErrorType::InvalidProperties,
                                "Expected a string value for \"charset\" field.".to_string(),
                            )
                        })?
                        .into(),
                );
            };
        }

        match (
            part.get("disposition").and_then(|v| v.to_string()),
            part.get("name").and_then(|v| v.to_string()),
        ) {
            (Some(disposition), Some(filename)) => {
                mime_part.headers.insert(
                    "Content-Disposition".into(),
                    ContentType::new(disposition)
                        .attribute("filename", filename)
                        .into(),
                );
            }
            (Some(disposition), None) => {
                mime_part.headers.insert(
                    "Content-Disposition".into(),
                    ContentType::new(disposition).into(),
                );
            }
            (None, Some(filename)) => {
                content_type
                    .attributes
                    .insert("name".into(), filename.into());
            }
            (None, None) => (),
        };

        if let Some(languages) = part.get("language").and_then(|v| v.to_array()) {
            mime_part.headers.insert(
                "Content-Language".into(),
                Text::new(
                    languages
                        .iter()
                        .filter_map(|v| v.to_string())
                        .collect::<Vec<&str>>()
                        .join(","),
                )
                .into(),
            );
        }

        if let Some(cid) = part.get("cid").and_then(|v| v.to_string()) {
            mime_part
                .headers
                .insert("Content-ID".into(), MessageId::new(cid).into());
        }

        if let Some(location) = part.get("location").and_then(|v| v.to_string()) {
            mime_part
                .headers
                .insert("Content-Location".into(), Text::new(location).into());
        }
    }

    mime_part
        .headers
        .insert("Content-Type".into(), content_type.into());

    for (property, value) in part {
        if property.starts_with("header:") {
            match property.split(':').nth(1) {
                Some(header_name) if !header_name.is_empty() => {
                    mime_part.headers.insert(
                        header_name.into(),
                        Raw::new(value.to_string().ok_or_else(|| {
                            JSONValue::new_error(
                                JMAPSetErrorType::InvalidProperties,
                                format!("Expected a string value for \"{}\" field.", property),
                            )
                        })?)
                        .into(),
                    );
                }
                _ => (),
            }
        }
    }

    if let Some(headers) = part.get("headers").and_then(|v| v.to_array()) {
        for header in headers {
            let header = header.to_object().ok_or_else(|| {
                JSONValue::new_error(
                    JMAPSetErrorType::InvalidProperties,
                    "Expected an object for \"headers\" field.".to_string(),
                )
            })?;
            mime_part.headers.insert(
                header
                    .get("name")
                    .and_then(|v| v.to_string())
                    .ok_or_else(|| {
                        JSONValue::new_error(
                            JMAPSetErrorType::InvalidProperties,
                            "Expected a string value for \"name\" field in \"headers\" field."
                                .to_string(),
                        )
                    })?
                    .into(),
                Raw::new(
                    header
                        .get("value")
                        .and_then(|v| v.to_string())
                        .ok_or_else(|| {
                            JSONValue::new_error(
                                JMAPSetErrorType::InvalidProperties,
                                "Expected a string value for \"value\" field in \"headers\" field."
                                    .to_string(),
                            )
                        })?,
                )
                .into(),
            );
        }
    }
    Ok((
        mime_part,
        if is_multipart {
            part.get("subParts").and_then(|v| v.to_array())
        } else {
            None
        },
    ))
}

fn import_body_parts<'x, 'y>(
    store: &impl JMAPLocalBlobStore<'y>,
    account: AccountId,
    parts: &'x JSONValue,
    body_values: Option<&'x HashMap<String, JSONValue>>,
    implicit_type: Option<&'x str>,
    strict_implicit_type: bool,
) -> Result<Vec<MimePart<'x>>, JSONValue> {
    let parts = parts.to_array().ok_or_else(|| {
        JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Expected an array containing body part.".to_string(),
        )
    })?;

    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        result.push(
            import_body_part(
                store,
                account,
                part,
                body_values,
                implicit_type,
                strict_implicit_type,
            )?
            .0,
        );
    }

    Ok(result)
}

fn import_header<'x, 'y>(
    builder: &mut MessageBuilder<'y>,
    header: HeaderName<'x>,
    form: JMAPMailHeaderForm,
    value: &'y JSONValue,
) -> Result<(), JSONValue> {
    match form {
        JMAPMailHeaderForm::Raw => {
            builder.header(header.unwrap(), Raw::new(import_json_string(value)?))
        }
        JMAPMailHeaderForm::Text => {
            builder.header(header.unwrap(), Text::new(import_json_string(value)?))
        }
        JMAPMailHeaderForm::Addresses => builder.header(
            header.unwrap(),
            Address::List(import_json_addresses(value)?),
        ),
        JMAPMailHeaderForm::GroupedAddresses => builder.header(
            header.unwrap(),
            Address::List(import_json_grouped_addresses(value)?),
        ),
        JMAPMailHeaderForm::MessageIds => builder.header(
            header.unwrap(),
            MessageId::from(import_json_string_list(value)?),
        ),
        JMAPMailHeaderForm::Date => {
            builder.header(header.unwrap(), Date::new(import_json_date(value)?))
        }
        JMAPMailHeaderForm::URLs => {
            builder.header(header.unwrap(), URL::from(import_json_string_list(value)?))
        }
    }
    Ok(())
}

fn import_json_string(value: &JSONValue) -> Result<&str, JSONValue> {
    value.to_string().ok_or_else(|| {
        JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Expected a String property.".to_string(),
        )
    })
}

fn import_json_date(value: &JSONValue) -> Result<i64, JSONValue> {
    Ok(
        DateTime::parse_from_rfc3339(value.to_string().ok_or_else(|| {
            JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                "Expected a Date property.".to_string(),
            )
        })?)
        .map_err(|_| {
            JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                "Expected a valid Date property.".to_string(),
            )
        })?
        .timestamp(),
    )
}

fn import_json_string_list(value: &JSONValue) -> Result<Vec<&str>, JSONValue> {
    let value = value.to_array().ok_or_else(|| {
        JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Expected an array with String.".to_string(),
        )
    })?;

    let mut list = Vec::with_capacity(value.len());
    for v in value {
        list.push(v.to_string().ok_or_else(|| {
            JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                "Expected an array with String.".to_string(),
            )
        })?);
    }

    Ok(list)
}

fn import_json_addresses(value: &JSONValue) -> Result<Vec<Address>, JSONValue> {
    let value = value.to_array().ok_or_else(|| {
        JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Expected an array with EmailAddress objects.".to_string(),
        )
    })?;

    let mut result = Vec::with_capacity(value.len());
    for addr in value {
        let addr = addr.to_object().ok_or_else(|| {
            JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                "Expected an array containing EmailAddress objects.".to_string(),
            )
        })?;
        result.push(Address::new_address(
            addr.get("name").and_then(|n| n.to_string()),
            addr.get("email")
                .and_then(|n| n.to_string())
                .ok_or_else(|| {
                    JSONValue::new_error(
                        JMAPSetErrorType::InvalidProperties,
                        "Missing 'email' field in EmailAddress object.".to_string(),
                    )
                })?,
        ));
    }

    Ok(result)
}

fn import_json_grouped_addresses(value: &JSONValue) -> Result<Vec<Address>, JSONValue> {
    let value = value.to_array().ok_or_else(|| {
        JSONValue::new_error(
            JMAPSetErrorType::InvalidProperties,
            "Expected an array with EmailAddressGroup objects.".to_string(),
        )
    })?;

    let mut result = Vec::with_capacity(value.len());
    for addr in value {
        let addr = addr.to_object().ok_or_else(|| {
            JSONValue::new_error(
                JMAPSetErrorType::InvalidProperties,
                "Expected an array containing EmailAddressGroup objects.".to_string(),
            )
        })?;
        result.push(Address::new_group(
            addr.get("name").and_then(|n| n.to_string()),
            import_json_addresses(addr.get("addresses").ok_or_else(|| {
                JSONValue::new_error(
                    JMAPSetErrorType::InvalidProperties,
                    "Missing 'addresses' field in EmailAddressGroup object.".to_string(),
                )
            })?)?,
        ));
    }

    Ok(result)
}
