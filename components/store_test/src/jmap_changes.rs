use std::collections::HashSet;

use jmap::changes::{JMAPChanges, JMAPState};
use store::{batch::WriteBatch, AccountId, Collection, JMAPId, JMAPStore, Store};

use crate::db_log::assert_compaction;

#[derive(Debug, Clone, Copy)]
pub enum LogAction {
    Insert(JMAPId),
    Update(JMAPId),
    Delete(JMAPId),
    UpdateChild(JMAPId),
    Move(JMAPId, JMAPId),
}

pub fn jmap_changes<T>(mail_store: &JMAPStore<T>, account_id: AccountId)
where
    T: for<'x> Store<'x> + 'static,
{
    let mut states = vec![JMAPState::Initial];

    for (changes, expected_changelog) in [
        (
            vec![
                LogAction::Insert(0),
                LogAction::Insert(1),
                LogAction::Insert(2),
            ],
            vec![vec![vec![0, 1, 2], vec![], vec![]]],
        ),
        (
            vec![
                LogAction::Move(0, 3),
                LogAction::Insert(4),
                LogAction::Insert(5),
                LogAction::Update(1),
                LogAction::Update(2),
            ],
            vec![
                vec![vec![1, 2, 3, 4, 5], vec![], vec![]],
                vec![vec![3, 4, 5], vec![1, 2], vec![0]],
            ],
        ),
        (
            vec![
                LogAction::Delete(1),
                LogAction::Insert(6),
                LogAction::Insert(7),
                LogAction::Update(2),
            ],
            vec![
                vec![vec![2, 3, 4, 5, 6, 7], vec![], vec![]],
                vec![vec![3, 4, 5, 6, 7], vec![2], vec![0, 1]],
                vec![vec![6, 7], vec![2], vec![1]],
            ],
        ),
        (
            vec![
                LogAction::Update(4),
                LogAction::Update(5),
                LogAction::Update(6),
                LogAction::Update(7),
            ],
            vec![
                vec![vec![2, 3, 4, 5, 6, 7], vec![], vec![]],
                vec![vec![3, 4, 5, 6, 7], vec![2], vec![0, 1]],
                vec![vec![6, 7], vec![2, 4, 5], vec![1]],
                vec![vec![], vec![4, 5, 6, 7], vec![]],
            ],
        ),
        (
            vec![
                LogAction::Delete(4),
                LogAction::Delete(5),
                LogAction::Delete(6),
                LogAction::Delete(7),
            ],
            vec![
                vec![vec![2, 3], vec![], vec![]],
                vec![vec![3], vec![2], vec![0, 1]],
                vec![vec![], vec![2], vec![1, 4, 5]],
                vec![vec![], vec![], vec![4, 5, 6, 7]],
                vec![vec![], vec![], vec![4, 5, 6, 7]],
            ],
        ),
        (
            vec![
                LogAction::Insert(8),
                LogAction::Insert(9),
                LogAction::Insert(10),
                LogAction::Update(3),
            ],
            vec![
                vec![vec![2, 3, 8, 9, 10], vec![], vec![]],
                vec![vec![3, 8, 9, 10], vec![2], vec![0, 1]],
                vec![vec![8, 9, 10], vec![2, 3], vec![1, 4, 5]],
                vec![vec![8, 9, 10], vec![3], vec![4, 5, 6, 7]],
                vec![vec![8, 9, 10], vec![3], vec![4, 5, 6, 7]],
                vec![vec![8, 9, 10], vec![3], vec![]],
            ],
        ),
        (
            vec![LogAction::Update(2), LogAction::Update(8)],
            vec![
                vec![vec![2, 3, 8, 9, 10], vec![], vec![]],
                vec![vec![3, 8, 9, 10], vec![2], vec![0, 1]],
                vec![vec![8, 9, 10], vec![2, 3], vec![1, 4, 5]],
                vec![vec![8, 9, 10], vec![2, 3], vec![4, 5, 6, 7]],
                vec![vec![8, 9, 10], vec![2, 3], vec![4, 5, 6, 7]],
                vec![vec![8, 9, 10], vec![2, 3], vec![]],
                vec![vec![], vec![2, 8], vec![]],
            ],
        ),
        (
            vec![
                LogAction::Move(9, 11),
                LogAction::Move(10, 12),
                LogAction::Delete(8),
            ],
            vec![
                vec![vec![2, 3, 11, 12], vec![], vec![]],
                vec![vec![3, 11, 12], vec![2], vec![0, 1]],
                vec![vec![11, 12], vec![2, 3], vec![1, 4, 5]],
                vec![vec![11, 12], vec![2, 3], vec![4, 5, 6, 7]],
                vec![vec![11, 12], vec![2, 3], vec![4, 5, 6, 7]],
                vec![vec![11, 12], vec![2, 3], vec![]],
                vec![vec![11, 12], vec![2], vec![8, 9, 10]],
                vec![vec![11, 12], vec![], vec![8, 9, 10]],
            ],
        ),
    ] {
        let mut documents = WriteBatch::new(account_id, false);

        for change in changes {
            match change {
                LogAction::Insert(id) => documents.log_insert(Collection::Mail, id),
                LogAction::Update(id) => documents.log_update(Collection::Mail, id),
                LogAction::Delete(id) => documents.log_delete(Collection::Mail, id),
                LogAction::UpdateChild(id) => documents.log_child_update(Collection::Mail, id),
                LogAction::Move(old_id, new_id) => {
                    documents.log_move(Collection::Mail, old_id, new_id)
                }
            }
        }

        mail_store.write(documents).unwrap();

        let mut new_state = JMAPState::Initial;
        for (test_num, state) in (&states).iter().enumerate() {
            let changes = mail_store
                .get_jmap_changes(account_id, Collection::Mail, state.clone(), 0)
                .unwrap();

            assert_eq!(
                expected_changelog[test_num],
                vec![changes.created, changes.updated, changes.destroyed]
                    .into_iter()
                    .map(|list| {
                        let mut list = list.into_iter().collect::<Vec<_>>();
                        list.sort_unstable();
                        list
                    })
                    .collect::<Vec<Vec<_>>>(),
                "test_num: {}, state: {:?}",
                test_num,
                state
            );

            if let JMAPState::Initial = state {
                new_state = changes.new_state;
            }

            for max_changes in 1..=8 {
                let mut insertions = expected_changelog[test_num][0]
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>();
                let mut updates = expected_changelog[test_num][1]
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>();
                let mut deletions = expected_changelog[test_num][2]
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>();

                let mut int_state = state.clone();

                for _ in 0..100 {
                    let changes = mail_store
                        .get_jmap_changes(account_id, Collection::Mail, int_state, max_changes)
                        .unwrap();

                    assert!(
                        changes.total_changes <= max_changes,
                        "{} > {}",
                        changes.total_changes,
                        max_changes
                    );

                    changes.created.iter().for_each(|id| {
                        assert!(insertions.remove(id));
                    });
                    changes.updated.iter().for_each(|id| {
                        assert!(updates.remove(id));
                    });
                    changes.destroyed.iter().for_each(|id| {
                        assert!(deletions.remove(id));
                    });

                    int_state = changes.new_state;

                    if !changes.has_more_changes {
                        break;
                    }
                }

                assert_eq!(insertions.len(), 0);
                assert_eq!(updates.len(), 0);
                assert_eq!(deletions.len(), 0);
            }
        }

        states.push(new_state);
    }

    assert_compaction(mail_store, 1);

    let changes = mail_store
        .get_jmap_changes(account_id, Collection::Mail, JMAPState::Initial, 0)
        .unwrap();

    assert_eq!(changes.created, vec![2, 3, 11, 12]);
    assert_eq!(changes.updated, Vec::<u64>::new());
    assert_eq!(changes.destroyed, Vec::<u64>::new());
}
