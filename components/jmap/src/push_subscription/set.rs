use crate::error::set::{SetError, SetErrorType};
use crate::from_timestamp;
use crate::id::jmap::JMAPId;
use crate::jmap_store::orm::{self, JMAPOrm, TinyORM};
use crate::jmap_store::set::SetHelper;
use crate::jmap_store::Object;
use crate::request::set::SetResponse;
use crate::{jmap_store::set::SetObject, request::set::SetRequest};
use store::chrono::Utc;
use store::parking_lot::MutexGuard;
use store::rand::distributions::Alphanumeric;
use store::rand::{thread_rng, Rng};
use store::{JMAPStore, Store};

use super::schema::{Property, PushSubscription, Value};

const EXPIRES_MAX: i64 = 7 * 24 * 3600; // 7 days
const MAX_SUBSCRIPTIONS: u64 = 100;
const VERIFICATION_CODE_LEN: usize = 32;

impl SetObject for PushSubscription {
    type SetArguments = ();

    type NextInvocation = ();

    fn map_references(&mut self, fnc: impl FnMut(&str) -> Option<JMAPId>) {
        todo!()
    }
}

pub trait JMAPSetPushSubscription<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn push_subscription_set(
        &self,
        request: SetRequest<PushSubscription>,
    ) -> crate::Result<SetResponse<PushSubscription>>;
}

impl<T> JMAPSetPushSubscription<T> for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn push_subscription_set(
        &self,
        request: SetRequest<PushSubscription>,
    ) -> crate::Result<SetResponse<PushSubscription>> {
        let mut helper = SetHelper::new(self, request)?;

        helper.create(|_create_id, item, helper, document| {
            // Limit the number of subscriptions
            if helper.document_ids.len() > MAX_SUBSCRIPTIONS {
                return Err(SetError::new(
                    SetErrorType::Forbidden,
                    "There are too many subscriptions, please delete some before adding a new one."
                        .to_string(),
                ));
            }

            let mut fields = TinyORM::<PushSubscription>::new();
            let mut expires = None;

            for (property, value) in item.properties {
                fields.set(
                    property,
                    match (property, value) {
                        (Property::DeviceClientId, value @ Value::Text { .. }) => {
                            orm::Value::Object(value)
                        }
                        (Property::Url, Value::Text { value })
                            if value.starts_with("https://") && value.len() < 512 =>
                        {
                            orm::Value::Object(Value::Text { value })
                        }
                        (Property::Keys, value @ Value::Keys { .. }) => orm::Value::Object(value),
                        (Property::Expires, Value::DateTime { value }) => {
                            expires = value.into();
                            continue;
                        }
                        (Property::Types, value @ Value::Types { .. }) => orm::Value::Object(value),
                        (
                            Property::Keys
                            | Property::Expires
                            | Property::Types
                            | Property::VerificationCode,
                            Value::Null,
                        ) => orm::Value::Null,
                        (property, _) => {
                            return Err(SetError::invalid_property(
                                property,
                                "Field could not be set.",
                            ));
                        }
                    },
                );
            }

            // Add expires
            let current_time = Utc::now().timestamp();
            let expires = expires
                .map(|dt| dt.timestamp())
                .unwrap_or_else(|| current_time + EXPIRES_MAX);
            fields.set(
                Property::Expires,
                orm::Value::Object(Value::DateTime {
                    value: from_timestamp(
                        if expires > current_time && (expires - current_time) > EXPIRES_MAX {
                            current_time + EXPIRES_MAX
                        } else {
                            expires
                        },
                    ),
                }),
            );

            // Generate random verification code
            fields.set(
                Property::VerificationCode_,
                orm::Value::Object(Value::Text {
                    value: thread_rng()
                        .sample_iter(Alphanumeric)
                        .take(VERIFICATION_CODE_LEN)
                        .map(char::from)
                        .collect::<String>(),
                }),
            );

            // Validate fields
            fields.insert_validate(document)?;

            Ok((
                PushSubscription::new(document.document_id.into()),
                None::<MutexGuard<'_, ()>>,
            ))
        })?;

        helper.update(|id, item, helper, document| {
            let current_fields = self
                .get_orm::<PushSubscription>(helper.account_id, id.get_document_id())?
                .ok_or_else(|| SetError::new_err(SetErrorType::NotFound))?;
            let mut fields = TinyORM::track_changes(&current_fields);
            let mut expires = None;

            for (property, value) in item.properties {
                fields.set(
                    property,
                    match (property, value) {
                        (Property::Expires, Value::DateTime { value }) => {
                            expires = value.timestamp().into();
                            continue;
                        }
                        (Property::Types, value @ Value::Types { .. }) => orm::Value::Object(value),
                        (Property::VerificationCode, Value::Text { value }) => {
                            if current_fields
                                .get_string(&Property::VerificationCode_)
                                .map_or(false, |v| v == value)
                            {
                                orm::Value::Object(Value::Text { value })
                            } else {
                                return Err(SetError::invalid_property(
                                    property,
                                    "Verification code does not match.".to_string(),
                                ));
                            }
                        }
                        (Property::Expires, Value::Null) => {
                            expires = (Utc::now().timestamp() + EXPIRES_MAX).into();
                            continue;
                        }
                        (Property::Types, Value::Null) => orm::Value::Null,
                        (property, _) => {
                            return Err(SetError::invalid_property(
                                property,
                                "Property cannot be set or an invalid value was provided.",
                            ));
                        }
                    },
                );
            }

            if let Some(expires) = expires {
                // Add expires
                let current_time = Utc::now().timestamp();
                fields.set(
                    Property::Expires,
                    orm::Value::Object(Value::DateTime {
                        value: from_timestamp(
                            if expires > current_time && (expires - current_time) > EXPIRES_MAX {
                                current_time + EXPIRES_MAX
                            } else {
                                expires
                            },
                        ),
                    }),
                );
            }

            // Merge changes
            current_fields.merge_validate(document, fields)?;
            Ok(None)
        })?;

        helper.destroy(|_id, helper, document| {
            if let Some(orm) =
                self.get_orm::<PushSubscription>(helper.account_id, document.document_id)?
            {
                orm.delete(document);
            }
            Ok(())
        })?;

        helper.into_response()
    }
}
