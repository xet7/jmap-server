/*
 * Copyright (c) 2020-2022, Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart JMAP Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use std::{fs, path::PathBuf};

use actix_web::web;

use jmap::types::jmap::JMAPId;
use jmap_client::{
    client::Client,
    email::{self, Header, HeaderForm},
    mailbox::Role,
};
use store::Store;

use crate::{
    tests::{
        jmap_mail::{email_get::all_headers, replace_blob_ids},
        store::utils::StoreCompareWith,
    },
    JMAPServer,
};

pub async fn test<T>(server: web::Data<JMAPServer<T>>, client: &mut Client)
where
    T: for<'x> Store<'x> + 'static,
{
    println!("Running Email Parse tests...");

    let mut test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    test_dir.push("src");
    test_dir.push("tests");
    test_dir.push("resources");
    test_dir.push("jmap_mail_parse");

    let mailbox_id = client
        .set_default_account_id(JMAPId::new(1).to_string())
        .mailbox_create("JMAP Parse", None::<String>, Role::None)
        .await
        .unwrap()
        .take_id();

    // Test parsing an email attachment
    for test_name in ["attachment.eml", "attachment_b64.eml"] {
        let mut test_file = test_dir.clone();
        test_file.push(test_name);

        let email = client
            .email_import(
                fs::read(&test_file).unwrap(),
                [mailbox_id.clone()],
                None::<Vec<String>>,
                None,
            )
            .await
            .unwrap();

        let blob_id = client
            .email_get(email.id().unwrap(), Some([email::Property::Attachments]))
            .await
            .unwrap()
            .unwrap()
            .attachments()
            .unwrap()
            .first()
            .unwrap()
            .blob_id()
            .unwrap()
            .to_string();

        let email = client
            .email_parse(
                &blob_id,
                [
                    email::Property::Id,
                    email::Property::BlobId,
                    email::Property::ThreadId,
                    email::Property::MailboxIds,
                    email::Property::Keywords,
                    email::Property::Size,
                    email::Property::ReceivedAt,
                    email::Property::MessageId,
                    email::Property::InReplyTo,
                    email::Property::References,
                    email::Property::Sender,
                    email::Property::From,
                    email::Property::To,
                    email::Property::Cc,
                    email::Property::Bcc,
                    email::Property::ReplyTo,
                    email::Property::Subject,
                    email::Property::SentAt,
                    email::Property::HasAttachment,
                    email::Property::Preview,
                    email::Property::BodyValues,
                    email::Property::TextBody,
                    email::Property::HtmlBody,
                    email::Property::Attachments,
                    email::Property::BodyStructure,
                ]
                .into(),
                [
                    email::BodyProperty::PartId,
                    email::BodyProperty::BlobId,
                    email::BodyProperty::Size,
                    email::BodyProperty::Name,
                    email::BodyProperty::Type,
                    email::BodyProperty::Charset,
                    email::BodyProperty::Headers,
                    email::BodyProperty::Disposition,
                    email::BodyProperty::Cid,
                    email::BodyProperty::Language,
                    email::BodyProperty::Location,
                ]
                .into(),
                100.into(),
            )
            .await
            .unwrap();

        if !test_name.contains("_b64") {
            for parts in [
                email.text_body().unwrap(),
                email.html_body().unwrap(),
                email.attachments().unwrap(),
            ] {
                for part in parts {
                    let blob_id = part.blob_id().unwrap();

                    let inner_blob = client.download(blob_id).await.unwrap();

                    test_file.set_extension(format!("part{}", part.part_id().unwrap()));

                    //fs::write(&test_file, inner_blob).unwrap();
                    let expected_inner_blob = fs::read(&test_file).unwrap();

                    assert_eq!(
                        inner_blob,
                        expected_inner_blob,
                        "file: {}",
                        test_file.display()
                    );
                }
            }
        }

        test_file.set_extension("json");

        let result = replace_blob_ids(serde_json::to_string_pretty(&email.into_test()).unwrap());

        if fs::read(&test_file).unwrap() != result.as_bytes() {
            test_file.set_extension("failed");
            fs::write(&test_file, result.as_bytes()).unwrap();
            panic!("Test failed, output saved to {}", test_file.display());
        }
    }

    // Test header parsing on a temporary blob
    let mut test_file = test_dir;
    test_file.push("headers.eml");
    let blob_id = client
        .upload(None, fs::read(&test_file).unwrap(), None)
        .await
        .unwrap()
        .take_blob_id();

    let mut email = client
        .email_parse(
            &blob_id,
            [
                email::Property::Id,
                email::Property::MessageId,
                email::Property::InReplyTo,
                email::Property::References,
                email::Property::Sender,
                email::Property::From,
                email::Property::To,
                email::Property::Cc,
                email::Property::Bcc,
                email::Property::ReplyTo,
                email::Property::Subject,
                email::Property::SentAt,
                email::Property::Preview,
                email::Property::TextBody,
                email::Property::HtmlBody,
                email::Property::Attachments,
            ]
            .into(),
            [
                email::BodyProperty::Size,
                email::BodyProperty::Name,
                email::BodyProperty::Type,
                email::BodyProperty::Charset,
                email::BodyProperty::Disposition,
                email::BodyProperty::Cid,
                email::BodyProperty::Language,
                email::BodyProperty::Location,
                email::BodyProperty::Header(Header {
                    name: "X-Custom-Header".into(),
                    form: HeaderForm::Raw,
                    all: false,
                }),
                email::BodyProperty::Header(Header {
                    name: "X-Custom-Header-2".into(),
                    form: HeaderForm::Raw,
                    all: false,
                }),
            ]
            .into(),
            100.into(),
        )
        .await
        .unwrap()
        .into_test();

    for property in all_headers() {
        email.headers.extend(
            client
                .email_parse(&blob_id, [property].into(), [].into(), None)
                .await
                .unwrap()
                .into_test()
                .headers,
        );
    }

    test_file.set_extension("json");

    let result = replace_blob_ids(serde_json::to_string_pretty(&email).unwrap());

    if fs::read(&test_file).unwrap() != result.as_bytes() {
        test_file.set_extension("failed");
        fs::write(&test_file, result.as_bytes()).unwrap();
        panic!("Test failed, output saved to {}", test_file.display());
    }

    client.mailbox_destroy(&mailbox_id, true).await.unwrap();

    server.store.assert_is_empty();
}
