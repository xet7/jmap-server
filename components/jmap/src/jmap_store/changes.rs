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

use super::Object;
use crate::{
    request::changes::{ChangesRequest, ChangesResponse},
    types::json_pointer::JSONPointerEval,
    types::state::JMAPState,
};
use store::{
    core::{collection::Collection, error::StoreError},
    log::changes::{Change, Query},
    AccountId, JMAPStore, Store,
};

impl JSONPointerEval for () {
    fn eval_json_pointer(
        &self,
        _ptr: &crate::types::json_pointer::JSONPointer,
    ) -> Option<Vec<u64>> {
        None
    }
}

pub trait ChangesObject: Object {
    type ChangesResponse: Default + JSONPointerEval;
}

pub trait JMAPChanges {
    fn get_state(&self, account: AccountId, collection: Collection) -> store::Result<JMAPState>;
    fn changes<O>(&self, request: ChangesRequest) -> crate::Result<ChangesResponse<O>>
    where
        O: ChangesObject;
}

impl<T> JMAPChanges for JMAPStore<T>
where
    T: for<'x> Store<'x> + 'static,
{
    fn get_state(&self, account: AccountId, collection: Collection) -> store::Result<JMAPState> {
        Ok(self
            .get_last_change_id(account, collection)?
            .map(JMAPState::Exact)
            .unwrap_or(JMAPState::Initial))
    }

    fn changes<O>(&self, request: ChangesRequest) -> crate::Result<ChangesResponse<O>>
    where
        O: ChangesObject,
    {
        let collection = O::collection();
        let max_changes = request.max_changes.unwrap_or(0);
        let max_changes = if self.config.changes_max_results > 0
            && self.config.changes_max_results < max_changes
        {
            self.config.changes_max_results
        } else {
            max_changes
        };

        let (items_sent, mut changelog) = match &request.since_state {
            JMAPState::Initial => {
                let changelog = self
                    .get_changes(request.account_id.into(), collection, Query::All)?
                    .unwrap();
                if changelog.changes.is_empty() && changelog.from_change_id == 0 {
                    return Ok(ChangesResponse::empty(request.account_id));
                }

                (0, changelog)
            }
            JMAPState::Exact(change_id) => (
                0,
                self.get_changes(
                    request.account_id.into(),
                    collection,
                    Query::Since(*change_id),
                )?
                .ok_or_else(|| {
                    StoreError::InvalidArguments(
                        "The specified stateId does could not be found.".to_string(),
                    )
                })?,
            ),
            JMAPState::Intermediate(intermediate_state) => {
                let mut changelog = self
                    .get_changes(
                        request.account_id.into(),
                        collection,
                        Query::RangeInclusive(intermediate_state.from_id, intermediate_state.to_id),
                    )?
                    .ok_or_else(|| {
                        StoreError::InvalidArguments(
                            "The specified stateId does could not be found.".to_string(),
                        )
                    })?;
                if intermediate_state.items_sent >= changelog.changes.len() {
                    (
                        0,
                        self.get_changes(
                            request.account_id.into(),
                            collection,
                            Query::Since(intermediate_state.to_id),
                        )?
                        .ok_or_else(|| {
                            StoreError::InvalidArguments(
                                "The specified stateId does could not be found.".to_string(),
                            )
                        })?,
                    )
                } else {
                    changelog.changes.drain(
                        (changelog.changes.len() - intermediate_state.items_sent)
                            ..changelog.changes.len(),
                    );
                    (intermediate_state.items_sent, changelog)
                }
            }
        };

        let has_more_changes = if max_changes > 0 && changelog.changes.len() > max_changes {
            changelog
                .changes
                .drain(0..(changelog.changes.len() - max_changes));
            true
        } else {
            false
        };

        let mut created = Vec::new();
        let mut updated = Vec::new();
        let mut destroyed = Vec::new();
        let mut items_changed = false;

        let total_changes = changelog.changes.len();
        if total_changes > 0 {
            for change in changelog.changes {
                match change {
                    Change::Insert(item) => created.push(item.into()),
                    Change::Update(item) => {
                        items_changed = true;
                        updated.push(item.into())
                    }
                    Change::Delete(item) => destroyed.push(item.into()),
                    Change::ChildUpdate(item) => updated.push(item.into()),
                };
            }
        }

        Ok(ChangesResponse {
            account_id: request.account_id,
            total_changes,
            has_children_changes: !updated.is_empty() && !items_changed,
            has_more_changes,
            old_state: request.since_state,
            new_state: if has_more_changes {
                JMAPState::new_intermediate(
                    changelog.from_change_id,
                    changelog.to_change_id,
                    items_sent + max_changes,
                )
            } else {
                JMAPState::new_exact(changelog.to_change_id)
            },
            created,
            updated,
            destroyed,
            arguments: O::ChangesResponse::default(),
        })
    }
}
