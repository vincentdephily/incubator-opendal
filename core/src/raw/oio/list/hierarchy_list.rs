// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::collections::HashSet;
use std::task::ready;
use std::task::Context;
use std::task::Poll;

use crate::raw::*;
use crate::*;

/// ToHierarchyLister will convert a flat list to hierarchy by filter
/// not needed entries.
///
/// # Notes
///
/// ToHierarchyLister filter entries after fetch entries. So it's possible
/// to return an empty vec. It doesn't mean the all pages have been
/// returned.
///
/// Please keep calling next until we returned `Ok(None)`
pub struct HierarchyLister<P> {
    lister: P,
    path: String,
    visited: HashSet<String>,
}

impl<P> HierarchyLister<P> {
    /// Create a new hierarchy lister
    pub fn new(lister: P, path: &str) -> HierarchyLister<P> {
        let path = if path == "/" {
            "".to_string()
        } else {
            path.to_string()
        };

        HierarchyLister {
            lister,
            path,
            visited: HashSet::default(),
        }
    }

    /// ## NOTES
    ///
    /// We take `&mut Entry` here because we need to perform modification on entry in the case like
    /// listing "a/" with existing key `a/b/c`.
    ///
    /// In this case, we got a key `a/b/c`, but we should return `a/b/` instead to keep the hierarchy.
    fn keep_entry(&mut self, e: &mut oio::Entry) -> bool {
        // If path is not started with prefix, drop it.
        //
        // Ideally, it should never happen. But we just tolerate
        // this state.
        if !e.path().starts_with(&self.path) {
            return false;
        }

        // Dir itself should not be returned in hierarchy page.
        if e.path() == self.path {
            return false;
        }

        let prefix_len = self.path.len();

        let idx = if let Some(idx) = e.path()[prefix_len..].find('/') {
            idx + prefix_len + 1
        } else {
            // If there is no `/` in path, it's a normal file, we
            // can return it directly.
            return true;
        };

        // idx == path.len() means it's contain only one `/` at the
        // end of path.
        if idx == e.path().len() {
            if !self.visited.contains(e.path()) {
                self.visited.insert(e.path().to_string());
            }
            return true;
        }

        // If idx < path.len() means that are more levels to come.
        // We should check the first dir path.
        let has = {
            let path = &e.path()[..idx];
            self.visited.contains(path)
        };
        if !has {
            let path = {
                let path = &e.path()[..idx];
                path.to_string()
            };

            e.set_path(&path);
            e.set_mode(EntryMode::DIR);
            self.visited.insert(path);

            return true;
        }

        false
    }
}

impl<P: oio::List> oio::List for HierarchyLister<P> {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<oio::Entry>>> {
        loop {
            let mut entry = match ready!(self.lister.poll_next(cx))? {
                Some(entry) => entry,
                None => return Poll::Ready(Ok(None)),
            };

            if self.keep_entry(&mut entry) {
                return Poll::Ready(Ok(Some(entry)));
            }
        }
    }
}

impl<P: oio::BlockingList> oio::BlockingList for HierarchyLister<P> {
    fn next(&mut self) -> Result<Option<oio::Entry>> {
        loop {
            let mut entry = match self.lister.next()? {
                Some(entry) => entry,
                None => return Ok(None),
            };

            if self.keep_entry(&mut entry) {
                return Ok(Some(entry));
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use std::collections::HashSet;
    use std::vec::IntoIter;

    use log::debug;
    use oio::BlockingList;

    use super::*;

    struct MockLister {
        inner: IntoIter<&'static str>,
    }

    impl MockLister {
        fn new(inner: Vec<&'static str>) -> Self {
            Self {
                inner: inner.into_iter(),
            }
        }
    }

    impl BlockingList for MockLister {
        fn next(&mut self) -> Result<Option<oio::Entry>> {
            let entry = self.inner.next().map(|path| {
                if path.ends_with('/') {
                    oio::Entry::new(path, Metadata::new(EntryMode::DIR))
                } else {
                    oio::Entry::new(path, Metadata::new(EntryMode::FILE))
                }
            });

            Ok(entry)
        }
    }

    #[test]
    fn test_blocking_list() -> Result<()> {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();

        let lister = MockLister::new(vec![
            "x/x/", "x/y/", "y/", "x/x/x", "y/y", "xy/", "z", "y/a",
        ]);
        let mut lister = HierarchyLister::new(lister, "");

        let mut entries = Vec::default();

        let mut set = HashSet::new();
        while let Some(e) = lister.next()? {
            debug!("got path {}", e.path());
            assert!(
                set.insert(e.path().to_string()),
                "duplicated value: {}",
                e.path()
            );

            entries.push(e)
        }

        assert_eq!(
            entries[0],
            oio::Entry::new("x/", Metadata::new(EntryMode::DIR))
        );
        assert_eq!(
            entries[1],
            oio::Entry::new("y/", Metadata::new(EntryMode::DIR))
        );
        assert_eq!(
            entries[2],
            oio::Entry::new("xy/", Metadata::new(EntryMode::DIR))
        );
        assert_eq!(
            entries[3],
            oio::Entry::new("z", Metadata::new(EntryMode::FILE))
        );

        Ok(())
    }
}
