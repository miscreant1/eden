// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! This sub module contains a Lua implementation of hooks

#![deny(warnings)]

use super::{ChangedFileType, Hook, HookChangeset, HookChangesetParents, HookContext,
            HookExecution, HookFile, HookRejectionInfo};
use super::errors::*;
use aclchecker::Identity;
use failure::Error;
use futures::{failed, Future};
use futures::future::{ok, result};
use futures_ext::{BoxFuture, FutureExt};
use hlua::{AnyLuaString, AnyLuaValue, Lua, LuaError, LuaFunctionCallError, LuaTable, PushGuard,
           TuplePushError, Void, function0, function1, function2};
use hlua_futures::{AnyFuture, LuaCoroutine, LuaCoroutineBuilder};
use linked_hash_map::LinkedHashMap;
use mononoke_types::FileType;
use regex::{Regex, RegexBuilder};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

const HOOK_START_CODE_BASE: &str = include_str!("hook_start_base.lua");

const HOOK_START_CODE_CS: &str = "
__hook_start = function(info, arg)
    return __hook_start_base(info, arg, function(arg, ctx)
        local files = {}

        -- translation to lua from mercurial's util.shortuser()
        local get_author_unixname = function(author)
            local ind = author:find('@')
            if ind then
                author = author:sub(1, ind - 1)
            end

            ind = author:find('<')
            if ind then
                author = author:sub(ind + 1)
            end

            ind = author:find(' ')
            if ind then
                author = author:sub(0, ind)
            end

            ind = author:find('%.')
            if ind then
                author = author:sub(0, ind)
            end

            return author
        end

        for _, file_data in ipairs(arg) do
            local file = __set_common_file_functions(file_data.path, file_data.type)

            if not file.is_deleted() then
                file.contains_string = function(s) return coroutine.yield(__contains_string(file.path, s)) end
                file.len = function() return coroutine.yield(__file_len(file.path)) end
                file.content = function() return coroutine.yield(__file_content(file.path)) end
                file.path_regex_match = function(p) return coroutine.yield(__regex_match(p, file.path)) end
            end
            files[#files+1] = file
        end

        ctx.info.author_unixname = get_author_unixname(ctx.info.author)
        ctx.files = files
        ctx.file_content = function(path) return coroutine.yield(__file_content(path)) end
        ctx.parse_commit_msg = function() return coroutine.yield(__parse_commit_msg()) end
        ctx.is_valid_reviewer = function(user) return coroutine.yield(__is_valid_reviewer(user)) end
        ctx.regex_match = function(pattern, s) return coroutine.yield(__regex_match(pattern, s)) end
    end)
end
";

const HOOK_START_CODE_FILE: &str = "
__hook_start = function(info, arg)
    return __hook_start_base(info, arg, function(arg, ctx)
        local file = __set_common_file_functions(arg.path, arg.type)

        if not file.is_deleted() then
            file.contains_string = function(s) return coroutine.yield(__contains_string(s)) end
            file.len = function() return coroutine.yield(__file_len()) end
            file.content = function() return coroutine.yield(__file_content()) end
            file.is_symlink = function() return coroutine.yield(__is_symlink()) end
            file.path_regex_match = function(p) return coroutine.yield(__regex_match(p, file.path)) end
        end
        ctx.file = file
        ctx.regex_match = function(pattern, s) return coroutine.yield(__regex_match(pattern, s)) end
    end)
end
";

#[derive(Clone, Debug)]
pub struct LuaHook {
    pub name: String,
    /// The Lua code of the hook
    pub code: String,
}

impl Hook<HookChangeset> for LuaHook {
    fn run(&self, context: HookContext<HookChangeset>) -> BoxFuture<HookExecution, Error> {
        let mut hook_info = hashmap! {
            "repo_name" => context.repo_name.to_string(),
            "author" => context.data.author.to_string(),
            "comments" => context.data.comments.to_string(),
        };
        match context.data.parents {
            HookChangesetParents::None => (),
            HookChangesetParents::One(ref parent1_hash) => {
                hook_info.insert("parent1_hash", parent1_hash.to_string());
            }
            HookChangesetParents::Two(ref parent1_hash, ref parent2_hash) => {
                hook_info.insert("parent1_hash", parent1_hash.to_string());
                hook_info.insert("parent2_hash", parent2_hash.to_string());
            }
        }
        let mut code = HOOK_START_CODE_CS.to_string();
        code.push_str(HOOK_START_CODE_BASE);
        code.push_str(&self.code);

        let files_map: HashMap<String, HookFile> = context
            .data
            .files
            .iter()
            .map(|file| (file.path.clone(), file.clone()))
            .collect();
        let files_map2 = files_map.clone();

        let contains_string = {
            move |path: String, string: String| -> Result<AnyFuture, Error> {
                match files_map.get(&path) {
                    Some(file) => {
                        let future = file.contains_string(&string)
                            .map_err(|err| {
                                LuaError::ExecutionError(format!(
                                    "failed to get file content: {}",
                                    err
                                ))
                            })
                            .map(|contains| AnyLuaValue::LuaBoolean(contains));
                        Ok(AnyFuture::new(future))
                    }
                    None => Ok(AnyFuture::new(ok(AnyLuaValue::LuaBoolean(false)))),
                }
            }
        };
        let contains_string = function2(contains_string);
        let file_content = {
            let context2 = context.clone();
            move |path: String| -> Result<AnyFuture, Error> {
                let future = context2
                    .data
                    .file_content(path)
                    .map_err(|err| {
                        LuaError::ExecutionError(format!("failed to get file content: {}", err))
                    })
                    .map(|opt| match opt {
                        Some(content) => AnyLuaValue::LuaAnyString(AnyLuaString(content.to_vec())),
                        None => AnyLuaValue::LuaNil,
                    });
                Ok(AnyFuture::new(future))
            }
        };
        let file_content = function1(file_content);
        let file_len = {
            move |path: String| -> Result<AnyFuture, Error> {
                match files_map2.get(&path) {
                    Some(file) => {
                        let future = file.len()
                            .map_err(|err| {
                                LuaError::ExecutionError(format!(
                                    "failed to get file content: {}",
                                    err
                                ))
                            })
                            .map(|len| AnyLuaValue::LuaNumber(len as f64));
                        Ok(AnyFuture::new(future))
                    }
                    None => Ok(AnyFuture::new(ok(AnyLuaValue::LuaBoolean(false)))),
                }
            }
        };
        let file_len = function1(file_len);
        let regex_match = {
            move |pattern: String, string: String| -> Result<AnyFuture, Error> {
                let future = cached_regex_match(pattern, string)
                    .map_err(|err| LuaError::ExecutionError(format!("invalid regex: {}", err)))
                    .map(|matched| AnyLuaValue::LuaBoolean(matched));

                Ok(AnyFuture::new(future))
            }
        };
        let regex_match = function2(regex_match);

        let parse_commit_msg = {
            cloned!(context);
            move || -> Result<AnyFuture, Error> {
                let parsed_commit_msg = parse_commit_message(&context.data.comments)
                    .into_iter()
                    .map(|(key, val)| {
                        (
                            AnyLuaValue::LuaAnyString(AnyLuaString(key.as_bytes().to_vec())),
                            val,
                        )
                    })
                    .collect();
                Ok(AnyFuture::new(ok(AnyLuaValue::LuaArray(parsed_commit_msg))))
            }
        };
        let parse_commit_msg = function0(parse_commit_msg);

        let reviewers_acl_checker = context.data.reviewers_acl_checker.clone();
        let is_valid_reviewer = {
            move |user: String| -> Result<AnyFuture, Error> {
                let user = Identity::with_user(&user);
                let valid = match *reviewers_acl_checker {
                    Some(ref reviewers_acl_checker) => reviewers_acl_checker.is_member(&[&user]),
                    None => false,
                };
                Ok(AnyFuture::new(ok(AnyLuaValue::LuaBoolean(valid))))
            }
        };
        let is_valid_reviewer = function1(is_valid_reviewer);

        let mut lua = Lua::new();
        lua.openlibs();
        lua.set("__contains_string", contains_string);
        lua.set("__file_len", file_len);
        lua.set("__file_content", file_content);
        lua.set("__parse_commit_msg", parse_commit_msg);
        lua.set("__is_valid_reviewer", is_valid_reviewer);
        lua.set("__regex_match", regex_match);
        let res: Result<(), Error> = lua.execute::<()>(&code)
            .map_err(|e| ErrorKind::HookParseError(e.to_string()).into());
        if let Err(e) = res {
            return failed(e).boxify();
        }
        // Note the lifetime becomes static as the into_get method moves the lua
        // and the later create moves it again into the coroutine
        let res: Result<LuaCoroutineBuilder<PushGuard<Lua<'static>>>, Error> = lua.into_get(
            "__hook_start",
        ).map_err(|_| panic!("No __hook_start"));
        let builder = match res {
            Ok(builder) => builder,
            Err(e) => return failed(e).boxify(),
        };

        let mut files = vec![];

        for f in context.data.files {
            let ty = match f.ty {
                ChangedFileType::Added => "added",
                ChangedFileType::Deleted => "deleted",
                ChangedFileType::Modified => "modified",
            };
            files.push(hashmap!{
                "path" => f.path,
                "type" => ty.to_string(),
            });
        }

        self.convert_coroutine_res(builder.create((hook_info, files)))
    }
}

impl Hook<HookFile> for LuaHook {
    fn run(&self, context: HookContext<HookFile>) -> BoxFuture<HookExecution, Error> {
        let hook_info = hashmap! {
            "repo_name" => context.repo_name.to_string(),
        };
        let mut code = HOOK_START_CODE_FILE.to_string();
        code.push_str(HOOK_START_CODE_BASE);
        code.push_str(&self.code);
        let contains_string = {
            cloned!(context);
            move |string: String| -> Result<AnyFuture, Error> {
                let future = context
                    .data
                    .contains_string(&string)
                    .map_err(|err| {
                        LuaError::ExecutionError(format!("failed to get file content: {}", err))
                    })
                    .map(|contains| AnyLuaValue::LuaBoolean(contains));
                Ok(AnyFuture::new(future))
            }
        };
        let contains_string = function1(contains_string);
        let file_content = {
            cloned!(context);
            move || -> Result<AnyFuture, Error> {
                let future = context
                    .data
                    .file_content()
                    .map_err(|err| {
                        LuaError::ExecutionError(format!("failed to get file content: {}", err))
                    })
                    .map(|content| AnyLuaValue::LuaAnyString(AnyLuaString(content.to_vec())));
                Ok(AnyFuture::new(future))
            }
        };
        let file_content = function0(file_content);
        let is_symlink = {
            cloned!(context);
            move || -> Result<AnyFuture, Error> {
                let future = context
                    .data
                    .file_type()
                    .map_err(|err| {
                        LuaError::ExecutionError(format!("failed to get file content: {}", err))
                    })
                    .map(|file_type| {
                        let is_symlink = match file_type {
                            FileType::Symlink => true,
                            _ => false,
                        };
                        AnyLuaValue::LuaBoolean(is_symlink)
                    });
                Ok(AnyFuture::new(future))
            }
        };
        let is_symlink = function0(is_symlink);
        let file_len = {
            cloned!(context);
            move || -> Result<AnyFuture, Error> {
                let future = context
                    .data
                    .len()
                    .map_err(|err| {
                        LuaError::ExecutionError(format!("failed to get file content: {}", err))
                    })
                    .map(|len| AnyLuaValue::LuaNumber(len as f64));
                Ok(AnyFuture::new(future))
            }
        };
        let file_len = function0(file_len);

        let regex_match = {
            move |pattern: String, string: String| -> Result<AnyFuture, Error> {
                let future = cached_regex_match(pattern, string)
                    .map_err(|err| LuaError::ExecutionError(format!("invalid regex: {}", err)))
                    .map(|matched| AnyLuaValue::LuaBoolean(matched));

                Ok(AnyFuture::new(future))
            }
        };
        let regex_match = function2(regex_match);

        let mut lua = Lua::new();
        lua.openlibs();
        lua.set("__contains_string", contains_string);
        lua.set("__file_len", file_len);
        lua.set("__file_content", file_content);
        lua.set("__is_symlink", is_symlink);
        lua.set("__regex_match", regex_match);
        let res: Result<(), Error> = lua.execute::<()>(&code)
            .map_err(|e| ErrorKind::HookParseError(e.to_string()).into());
        if let Err(e) = res {
            return failed(e).boxify();
        }
        // Note the lifetime becomes static as the into_get method moves the lua
        // and the later create moves it again into the coroutine
        let res: Result<LuaCoroutineBuilder<PushGuard<Lua<'static>>>, Error> = lua.into_get(
            "__hook_start",
        ).map_err(|_| panic!("No __hook_start"));
        let builder = match res {
            Ok(builder) => builder,
            Err(e) => return failed(e).boxify(),
        };
        let ty = match context.data.ty {
            ChangedFileType::Added => "added".to_string(),
            ChangedFileType::Deleted => "deleted".to_string(),
            ChangedFileType::Modified => "modified".to_string(),
        };
        let data = hashmap!{
            "path" => context.data.path.clone(),
            "type" => ty,
        };
        self.convert_coroutine_res(builder.create((hook_info, data)))
    }
}

impl LuaHook {
    pub fn new(name: String, code: String) -> LuaHook {
        LuaHook { name, code }
    }

    fn convert_coroutine_res(
        &self,
        res: Result<
            LuaCoroutine<PushGuard<Lua<'static>>, LuaTable<PushGuard<Lua<'static>>>>,
            LuaFunctionCallError<TuplePushError<Void, Void>>,
        >,
    ) -> BoxFuture<HookExecution, Error> {
        let res = res.map_err(|err| ErrorKind::HookRuntimeError(format!("{:#?}", err)));
        try_boxfuture!(res)
            .map_err(move |err| Error::from(ErrorKind::HookRuntimeError(format!("{:#?}", err))))
            .map(|mut t| {
                t.get::<bool, _, _>(1)
                    .ok_or(ErrorKind::HookRuntimeError("No hook return".to_string()).into())
                    .map(|acc| {
                        if acc {
                            HookExecution::Accepted
                        } else {
                            let desc = match t.get::<String, _, _>(2) {
                                Some(desc) => desc,
                                None => "".into(),
                            };
                            let long_desc = match t.get::<String, _, _>(3) {
                                Some(long_desc) => long_desc,
                                None => "".into(),
                            };
                            HookExecution::Rejected(HookRejectionInfo::new(desc, long_desc))
                        }
                    })
            })
            .flatten()
            .boxify()
    }
}

fn cached_regex_match(
    pattern: String,
    string: String,
) -> impl Future<Item = bool, Error = regex::Error> {
    const REGEX_SIZE_LIMIT: usize = 10 * 1024;
    const REGEX_CACHE_SIZE: usize = 128;

    lazy_static! {
        static ref hook_regex_cache: Arc<RwLock<LinkedHashMap<String, Regex>>> = Arc::new(RwLock::new(LinkedHashMap::with_capacity(REGEX_CACHE_SIZE)));
    }

    let future = if let Some(r) = hook_regex_cache.read().unwrap().get(&pattern) {
        ok(r.is_match(&string)).left_future()
    } else {
        result(
            RegexBuilder::new(&pattern)
                .size_limit(REGEX_SIZE_LIMIT)
                .build(),
        ).and_then(move |r| {
            if hook_regex_cache.read().unwrap().len() > REGEX_CACHE_SIZE {
                hook_regex_cache.write().unwrap().pop_front();
            }
            hook_regex_cache.write().unwrap().insert(pattern, r.clone());
            ok(r.is_match(&string))
        })
            .right_future()
    };

    future
}

fn parse_commit_message(commit_msg: &str) -> HashMap<String, AnyLuaValue> {
    #[derive(Clone)]
    enum PhabricatorTagValueType {
        String,
        List,
    }

    impl PhabricatorTagValueType {
        fn convert(&self, value: &str) -> AnyLuaValue {
            match self {
                &PhabricatorTagValueType::String => to_lua_string(value.trim()),
                &PhabricatorTagValueType::List => {
                    let reviewers = SPLIT_USERNAMES
                        .split(value.trim())
                        .filter(|s| !s.is_empty());

                    to_lua_array(reviewers)
                }
            }
        }
    }

    struct PhabricatorTag {
        ty: PhabricatorTagValueType,
        name: String,
        value: String,
    }

    impl PhabricatorTag {
        pub fn new(name: &str, ty: PhabricatorTagValueType) -> Self {
            Self {
                ty,
                name: name.to_lowercase(),
                value: String::new(),
            }
        }

        pub fn append_line(&mut self, line: &str) {
            self.value.push_str(line);
            self.value.push('\n');
        }

        pub fn get_name(&self) -> String {
            self.name.to_lowercase()
        }

        pub fn get_value(&self) -> AnyLuaValue {
            self.ty.convert(&self.value)
        }
    }

    lazy_static! {
        static ref PHABRICATOR_TAGS: HashMap<&'static str, PhabricatorTagValueType> = hashmap!{
            "cc" => PhabricatorTagValueType::List,
            "subscribers" => PhabricatorTagValueType::List,
            "differential revision" => PhabricatorTagValueType::String,
            "revert plan" => PhabricatorTagValueType::String,
            "reviewed by" => PhabricatorTagValueType::List,
            "reviewers" => PhabricatorTagValueType::List,
            "summary" => PhabricatorTagValueType::String,
            "signature" => PhabricatorTagValueType::String,
            "tasks" => PhabricatorTagValueType::List,
            "test plan" => PhabricatorTagValueType::String,
        };

        // Phabricator tags starts with name of the tag (case insensitive, so both "test plan:"
        // "Test Plan:" also works), which should be at the beginning of the line.
        // Test plan ends either with another phabricator tag ("Reviewed by:", "CC:" etc")
        // or with a end of a commit message

        static ref SPLIT_USERNAMES: Regex = RegexBuilder::new("[\\s,]+")
            .case_insensitive(true)
            .build()
            .unwrap();
    }

    let lines = commit_msg.lines();
    let mut result = hashmap!{};

    let mut current_tag = PhabricatorTag::new("title", PhabricatorTagValueType::String);

    for line in lines {
        let mut maybe_tag_name_and_value = line.splitn(2, ":");

        let maybe_tag = maybe_tag_name_and_value
            .next()
            .map(|tag| tag.to_lowercase());
        let maybe_value = maybe_tag_name_and_value.next();
        match (maybe_tag, maybe_value) {
            (Some(ref tag), Some(value)) if PHABRICATOR_TAGS.contains_key(tag.as_str()) => {
                result.insert(current_tag.get_name(), current_tag.get_value());

                let current_tag_type = PHABRICATOR_TAGS.get(tag.as_str()).unwrap().clone();
                current_tag = PhabricatorTag::new(tag, current_tag_type);
                current_tag.append_line(value);
            }
            _ => {
                current_tag.append_line(line);
            }
        };
    }
    result.insert(current_tag.get_name(), current_tag.get_value());

    result
}

fn to_lua_string(s: &str) -> AnyLuaValue {
    AnyLuaValue::LuaAnyString(AnyLuaString(s.as_bytes().to_vec()))
}

fn to_lua_array<'a, T: IntoIterator<Item = &'a str>>(v: T) -> AnyLuaValue {
    let v: Vec<_> = v.into_iter()
        .enumerate()
        .map(|(i, val)| {
            (
                AnyLuaValue::LuaNumber((i + 1) as f64),
                AnyLuaValue::LuaString(val.to_string()),
            )
        })
        .collect();
    AnyLuaValue::LuaArray(v)
}

#[cfg(test)]
mod test {
    use super::*;
    use super::super::{ChangedFileType, HookChangeset, HookChangesetParents,
                       InMemoryFileContentStore};
    use aclchecker::AclChecker;
    use async_unit;
    use bytes::Bytes;
    use futures::Future;
    use mercurial_types::HgChangesetId;
    use std::str::FromStr;
    use std::sync::Arc;
    use test::to_mpath;

    #[test]
    fn test_parse_commit_msg() {
        fn check_parse_commit(commit_msg: &str, expected: HashMap<String, AnyLuaValue>) {
            assert_eq!(parse_commit_message(commit_msg), expected);
        }

        check_parse_commit(
            "mononoke: fix bug\nSummary: fix\nTest Plan: testinprod",
            hashmap!{
                "title".to_string() => to_lua_string("mononoke: fix bug"),
                "summary".to_string() => to_lua_string("fix"),
                "test plan".to_string() => to_lua_string("testinprod"),
            },
        );

        // multiline title
        check_parse_commit(
            "mononoke: fix bug\nsecondline\nSummary: fix\nTest Plan: testinprod",
            hashmap!{
                "title".to_string() => to_lua_string("mononoke: fix bug\nsecondline"),
                "summary".to_string() => to_lua_string("fix"),
                "test plan".to_string() => to_lua_string("testinprod"),
            },
        );

        check_parse_commit(
            "Summary: fix\nTest Plan: testinprod",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "summary".to_string() => to_lua_string("fix"),
                "test plan".to_string() => to_lua_string("testinprod"),
            },
        );

        // Tag should start at beginning of the line
        check_parse_commit(
            "Summary: fix\n Test Plan: testinprod",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "summary".to_string() => to_lua_string("fix\n Test Plan: testinprod"),
            },
        );

        check_parse_commit(
            "Summary: fix\nnot a tag: testinprod",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "summary".to_string() => to_lua_string("fix\nnot a tag: testinprod"),
            },
        );

        check_parse_commit(
            "Summary: fix\nFixed\na\nbug",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "summary".to_string() => to_lua_string("fix\nFixed\na\nbug"),
            },
        );

        check_parse_commit(
            "Summary: fix\nCC:",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "summary".to_string() => to_lua_string("fix"),
                "cc".to_string() => to_lua_array(vec![]),
            },
        );

        check_parse_commit(
            "CC: user1, user2, user3",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "cc".to_string() => to_lua_array(vec!["user1", "user2", "user3"]),
            },
        );

        check_parse_commit(
            "Tasks: T1111, T2222, T3333",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "tasks".to_string() => to_lua_array(vec!["T1111", "T2222", "T3333"]),
            },
        );

        check_parse_commit(
            "Summary: fix\nTest Plan: testinprod\n\nReviewed By: stash, luk, simonfar, tfox, anastasiyaz, aslpavel, jsgf",
            hashmap!{
                "title".to_string() => to_lua_string(""),
                "summary".to_string() => to_lua_string("fix"),
                "test plan".to_string() => to_lua_string("testinprod"),
                "reviewed by".to_string() => to_lua_array(vec!["stash", "luk", "simonfar", "tfox", "anastasiyaz", "aslpavel", "jsgf"]),
            },
        );

        check_parse_commit(
            "mononoke: fix fixovich
Summary:

fix
of a mononoke
bug

Test Plan: testinprod
Reviewed By: stash
Reviewers: #mononoke,
CC: jsgf
Tasks: T1234
Differential Revision: https://url/D123
",
            hashmap!{
                "title".to_string() => to_lua_string("mononoke: fix fixovich"),
                "summary".to_string() => to_lua_string("fix\nof a mononoke\nbug"),
                "test plan".to_string() => to_lua_string("testinprod"),
                "reviewed by".to_string() => to_lua_array(vec!["stash"]),
                "reviewers".to_string() => to_lua_array(vec!["#mononoke"]),
                "cc".to_string() => to_lua_array(vec!["jsgf"]),
                "tasks".to_string() => to_lua_array(vec!["T1234"]),
                "differential revision".to_string() =>  to_lua_string("https://url/D123"),
            },
        );

        // Parse (almost) a real commit message
        check_parse_commit(
            "mononoke: log error only once

Summary:
Previously `log_with_msg()` was logged twice if msg wasn't None - with and
without the message. This diff fixes it.

#accept2ship
Test Plan: buck check

Reviewers: simonfar, #mononoke

Reviewed By: simonfar

Subscribers: jsgf

Differential Revision: https://phabricator.intern.facebook.com/D1111111

Signature: 111111111:1111111111:bbbbbbbbbbbbbbbb",
            hashmap!{
                                        "title".to_string() => to_lua_string("mononoke: log error only once"),
                                        "summary".to_string() => to_lua_string(
                                            "Previously `log_with_msg()` was logged twice if msg wasn't None - with and\n\
            without the message. This diff fixes it.\n\
            \n\
            #accept2ship"),
                                        "test plan".to_string() => to_lua_string("buck check"),
                                        "reviewed by".to_string() => to_lua_array(vec!["simonfar"]),
                                        "reviewers".to_string() => to_lua_array(vec!["simonfar", "#mononoke"]),
                                        "subscribers".to_string() => to_lua_array(vec!["jsgf"]),
                                        "differential revision".to_string() =>  to_lua_string("https://phabricator.intern.facebook.com/D1111111"),
                                        "signature".to_string() =>  to_lua_string("111111111:1111111111:bbbbbbbbbbbbbbbb"),
                                    },
        );
    }

    #[test]
    fn test_cs_hook_simple_rejected() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_cs_hook_reviewers() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 local reviewers = ctx.parse_commit_msg()['reviewers']\n\
                 return not reviewers\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );

            let cs_id =
                HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
            let content_store = InMemoryFileContentStore::new();
            let reviewers_acl_checker = acl_checker();
            let hcs = HookChangeset::new(
                "some-author".into(),
                vec![],
                "blah blah blah\nReviewed By: user1, user2".into(),
                HookChangesetParents::One("p1-hash".into()),
                cs_id,
                Arc::new(content_store),
                reviewers_acl_checker,
            );
            let code = String::from(
                "hook = function (ctx)\n\
                 local reviewers = ctx.parse_commit_msg()['reviewed by']\n\
                 return #reviewers == 2\n\
                 end",
            );
            assert_matches!(run_changeset_hook(code, hcs), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_cs_hook_test_plan() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 local test_plan = ctx.parse_commit_msg()['test plan']\n\
                 return not test_plan\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );

            let cs_id =
                HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
            let content_store = InMemoryFileContentStore::new();
            let reviewers_acl_checker = acl_checker();
            let hcs = HookChangeset::new(
                "some-author".into(),
                vec![],
                "blah blah blah\nTest Plan: testinprod".into(),
                HookChangesetParents::One("p1-hash".into()),
                cs_id,
                Arc::new(content_store),
                reviewers_acl_checker,
            );
            let code = String::from(
                "hook = function (ctx)\n\
                 local test_plan = ctx.parse_commit_msg()['test plan']\n\
                 return test_plan == 'testinprod'\n\
                 end",
            );
            assert_matches!(run_changeset_hook(code, hcs), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_cs_hook_author_unixname() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.author_unixname == 'some-author'\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );

            let cs_id =
                HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
            let content_store = InMemoryFileContentStore::new();
            let reviewers_acl_checker = acl_checker();
            let hcs = HookChangeset::new(
                "Stanislau Hlebik <stash@fb.com>".into(),
                vec![],
                "blah blah blah\nTest Plan: testinprod".into(),
                HookChangesetParents::One("p1-hash".into()),
                cs_id,
                Arc::new(content_store),
                reviewers_acl_checker,
            );
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.author_unixname == 'stash'\n\
                 end",
            );
            assert_matches!(run_changeset_hook(code, hcs), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_cs_hook_valid_reviewer() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.is_valid_reviewer('tfox')\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_not_valid_reviewer() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return not ctx.is_valid_reviewer('uyqdyqduygqwduygqwuydgqdfgbducbe2ubjweuhqwudh37')\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_rejected_short_and_long_desc() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false, \"emus\", \"ostriches\"\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Rejected(HookRejectionInfo{ref description,
                    ref long_description}))
                    if description==&"emus" && long_description==&"ostriches"
            );
        });
    }

    #[test]
    fn test_cs_hook_author() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.author == \"some-author\"\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_file_paths() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            // Arrays passed from rust -> lua appear to be 1 indexed in Lua land
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.files[0] == nil and ctx.files[1].path == \"file1\" and\n\
                 ctx.files[2].path == \"file2\" and ctx.files[3].path == \"file3\" and\n\
                 ctx.files[6] == nil\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_file_contains_string_match() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.files[1].contains_string(\"file1sausages\") and\n
                 ctx.files[2].contains_string(\"file2sausages\") and\n
                 ctx.files[3].contains_string(\"file3sausages\")\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_path_regex_match() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.files[1].path_regex_match(\"file[0-9]\") and\n
                 ctx.files[2].path_regex_match(\"f*2\") and\n
                 ctx.files[3].path_regex_match(\"fil.3\")\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_regex_match() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.regex_match(\"file[0-9]\", ctx.files[1].path) and\n
                 ctx.regex_match(\"f*2\", ctx.files[2].path) and\n
                 ctx.regex_match(\"fil.3\", ctx.files[3].path)\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_file_content_match() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.files[1].content() == \"file1sausages\" and\n
                 ctx.files[2].content() == \"file2sausages\" and\n
                 ctx.files[3].content() == \"file3sausages\" and\n
                 ctx.files[5].content() == \"modifiedsausages\"\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_other_file_content_match() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file_content(\"file1\") == \"file1sausages\" and\n
                 ctx.file_content(\"file2\") == \"file2sausages\" and\n
                 ctx.file_content(\"file3\") == \"file3sausages\"\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_file_content_not_found_returns_nil() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file_content(\"no/such/path\") == nil\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_check_type() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 local added_file = ctx.files[1]
                 local added = added_file.is_added() and \
                    not added_file.is_deleted() and not added_file.is_modified()

                 local deleted_file = ctx.files[4]
                 local deleted = not deleted_file.is_added() and \
                    deleted_file.is_deleted() and not deleted_file.is_modified()

                 local modified_file = ctx.files[5]
                 local modified = not modified_file.is_added() and \
                    not modified_file.is_deleted() and modified_file.is_modified()

                 return added and deleted and modified
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_deleted() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 for _, f in ipairs(ctx.files) do
                    if f.is_deleted() then
                        return f.path == \"deleted\"\n
                    end
                 end
                 return false
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_file_len() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.files[1].len() == 13 and\n
                 ctx.files[2].len() == 13 and\n
                 ctx.files[3].len() == 13 and\n
                 ctx.files[5].len() == 16\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_comments() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.comments == \"some-comments\"\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_repo_name() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.repo_name == \"some-repo\"\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_one_parent() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.parent1_hash == \"p1-hash\" and \n\
                 ctx.info.parent2_hash == nil\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_two_parents() {
        async_unit::tokio_unit_test(|| {
            let mut changeset = default_changeset();
            changeset.parents = HookChangesetParents::Two("p1-hash".into(), "p2-hash".into());
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.parent1_hash == \"p1-hash\" and \n\
                 ctx.info.parent2_hash == \"p2-hash\"\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_no_parents() {
        async_unit::tokio_unit_test(|| {
            let mut changeset = default_changeset();
            changeset.parents = HookChangesetParents::None;
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.parent1_hash == nil and \n\
                 ctx.info.parent2_hash == nil\n\
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_cs_hook_no_hook_func() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "elephants = function (ctx)\n\
                 return true\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref msg)) if msg.contains("no hook function")
             );
        });
    }

    #[test]
    fn test_cs_hook_invalid_hook() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from("invalid code");
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookParseError(ref err_msg))
                    if err_msg.starts_with("Syntax error:")
             );
        });
    }

    #[test]
    fn test_cs_hook_exception() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 if ctx.info.author == \"some-author\" then\n\
                 error(\"fubar\")\n\
                 end\n\
                 return true\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.starts_with("LuaError")
             );
        });
    }

    #[test]
    fn test_cs_hook_invalid_return_val() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return \"aardvarks\"\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid hook return type")
             );
        });
    }

    #[test]
    fn test_cs_hook_invalid_short_desc() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false, 23, \"long desc\"\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid hook failure short description type")
            );
        });
    }

    #[test]
    fn test_cs_hook_invalid_long_desc() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false, \"short desc\", 23\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid hook failure long description type")
            );
        });
    }

    #[test]
    fn test_cs_hook_desc_when_hooks_is_accepted() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return true, \"short\", \"long\"\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("failure description must only be set if hook fails")
             );
        });
    }

    #[test]
    fn test_cs_hook_long_desc_when_hooks_is_accepted() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return true, nil, \"long\"\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_changeset_hook(code, changeset).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("failure long description must only be set if hook fails")
             );
        });
    }

    #[test]
    fn test_cs_hook_no_io_nor_os() {
        async_unit::tokio_unit_test(|| {
            let changeset = default_changeset();
            let code = String::from(
                "hook = function (ctx)\n\
                 return io == nil and os == nil\n
                 end",
            );
            assert_matches!(
                run_changeset_hook(code, changeset),
                Ok(HookExecution::Accepted)
            );
        });
    }

    #[test]
    fn test_file_hook_path() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.path == \"/a/b/c.txt\"\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_contains_string_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.contains_string(\"sausages\")\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_contains_string_no_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.contains_string(\"gerbils\")\n\
                 end",
            );
            assert_matches!(
                run_file_hook(code, hook_file),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_file_hook_path_regex_match_no_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.path_regex_match(\"a[0-9]bcde\")\n\
                 end",
            );
            assert_matches!(
                run_file_hook(code, hook_file),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_file_hook_regex_match_no_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.regex_match(\"a[0-9]bcde\", ctx.file.path)\n\
                 end",
            );
            assert_matches!(
                run_file_hook(code, hook_file),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_file_hook_path_regex_match_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.path_regex_match(\"a*.txt\")\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_regex_match_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.regex_match(\"a*.txt\", ctx.file.path)\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_path_regex_match_invalid_regex() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.path_regex_match(\"[0-\")\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid regex")
             );
        });
    }

    #[test]
    fn test_file_hook_regex_match_invalid_regex() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.regex_match(\"[0-\", ctx.file.path)\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid regex")
             );
        });
    }

    #[test]
    fn test_file_hook_content_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.content() == \"sausages\"\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_is_symlink() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_symlink_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.is_symlink()\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_is_not_symlink() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.is_symlink()\n\
                 end",
            );
            assert_matches!(
                run_file_hook(code, hook_file),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_file_hook_removed() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_removed_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.path == \"/a/b/c.txt\" and ctx.file.is_deleted()\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_len_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.len() == 8\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_len_no_matches() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.file.len() == 123\n\
                 end",
            );
            assert_matches!(
                run_file_hook(code, hook_file),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_file_hook_repo_name() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return ctx.info.repo_name == \"some-repo\"\n\
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    #[test]
    fn test_file_hook_rejected() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false\n\
                 end",
            );
            assert_matches!(
                run_file_hook(code, hook_file),
                Ok(HookExecution::Rejected(_))
            );
        });
    }

    #[test]
    fn test_file_hook_no_hook_func() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "elephants = function (ctx)\n\
                 return true\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg)) if err_msg.contains("no hook function")
             );
        });
    }

    #[test]
    fn test_file_hook_invalid_hook() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from("invalid code");
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookParseError(ref err_msg))
                    if err_msg.starts_with("Syntax error:")
             );
        });
    }

    #[test]
    fn test_file_hook_exception() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 if ctx.file.path == \"/a/b/c.txt\" then\n\
                 error(\"fubar\")\n\
                 end\n\
                 return true\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.starts_with("LuaError")
             );
        });
    }

    #[test]
    fn test_file_hook_invalid_return_val() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return \"aardvarks\"\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid hook return type")
             );
        });
    }

    #[test]
    fn test_file_hook_invalid_short_desc() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false, 23, \"long desc\"\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid hook failure short description type")
            );
        });
    }

    #[test]
    fn test_file_hook_invalid_long_desc() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return false, \"short desc\", 23\n\
                 end",
            );
            assert_matches!(
                err_downcast!(run_file_hook(code, hook_file).unwrap_err(), err: ErrorKind => err),
                Ok(ErrorKind::HookRuntimeError(ref err_msg))
                    if err_msg.contains("invalid hook failure long description type")
            );
        });
    }

    #[test]
    fn test_file_hook_no_io_nor_os() {
        async_unit::tokio_unit_test(|| {
            let hook_file = default_hook_added_file();
            let code = String::from(
                "hook = function (ctx)\n\
                 return io == nil and os == nil\n
                 end",
            );
            assert_matches!(run_file_hook(code, hook_file), Ok(HookExecution::Accepted));
        });
    }

    fn run_changeset_hook(code: String, changeset: HookChangeset) -> Result<HookExecution, Error> {
        let hook = LuaHook::new(String::from("testhook"), code.to_string());
        let context = HookContext::new(hook.name.clone(), "some-repo".into(), changeset);
        hook.run(context).wait()
    }

    fn run_file_hook(code: String, hook_file: HookFile) -> Result<HookExecution, Error> {
        let hook = LuaHook::new(String::from("testhook"), code.to_string());
        let context = HookContext::new(hook.name.clone(), "some-repo".into(), hook_file);
        hook.run(context).wait()
    }

    fn default_changeset() -> HookChangeset {
        let added = vec!["file1".into(), "file2".into(), "file3".into()];
        let deleted = vec!["deleted".into()];
        let modified = vec!["modified".into()];
        create_hook_changeset(added, deleted, modified)
    }

    fn create_hook_changeset(
        added: Vec<String>,
        deleted: Vec<String>,
        modified: Vec<String>,
    ) -> HookChangeset {
        let mut content_store = InMemoryFileContentStore::new();
        let cs_id = HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
        for path in added.iter().chain(modified.iter()) {
            let content = path.clone() + "sausages";
            let content_bytes: Bytes = content.into();
            content_store.insert(
                (cs_id.clone(), to_mpath(&path)),
                (FileType::Regular, content_bytes.into()),
            );
        }
        let content_store = Arc::new(content_store);
        let content_store2 = content_store.clone();

        let create_hook_files = move |files: Vec<String>, ty: ChangedFileType| -> Vec<HookFile> {
            files
                .into_iter()
                .map(|path| HookFile::new(path.clone(), content_store.clone(), cs_id, ty.clone()))
                .collect()
        };

        let mut hook_files = vec![];
        hook_files.extend(create_hook_files(added, ChangedFileType::Added));
        hook_files.extend(create_hook_files(deleted, ChangedFileType::Deleted));
        hook_files.extend(create_hook_files(modified, ChangedFileType::Modified));
        let reviewers_acl_checker = acl_checker();
        HookChangeset::new(
            "some-author".into(),
            hook_files,
            "some-comments".into(),
            HookChangesetParents::One("p1-hash".into()),
            cs_id,
            content_store2,
            reviewers_acl_checker,
        )
    }

    fn default_hook_symlink_file() -> HookFile {
        let mut content_store = InMemoryFileContentStore::new();
        let cs_id = HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
        content_store.insert(
            (cs_id.clone(), to_mpath("/a/b/c.txt")),
            (FileType::Symlink, "sausages".into()),
        );
        HookFile::new(
            "/a/b/c.txt".into(),
            Arc::new(content_store),
            cs_id,
            ChangedFileType::Added,
        )
    }

    fn default_hook_added_file() -> HookFile {
        let mut content_store = InMemoryFileContentStore::new();
        let cs_id = HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
        content_store.insert(
            (cs_id.clone(), to_mpath("/a/b/c.txt")),
            (FileType::Regular, "sausages".into()),
        );
        HookFile::new(
            "/a/b/c.txt".into(),
            Arc::new(content_store),
            cs_id,
            ChangedFileType::Added,
        )
    }

    fn default_hook_removed_file() -> HookFile {
        let content_store = InMemoryFileContentStore::new();
        let cs_id = HgChangesetId::from_str("473b2e715e0df6b2316010908879a3c78e275dd9").unwrap();
        HookFile::new(
            "/a/b/c.txt".into(),
            Arc::new(content_store),
            cs_id,
            ChangedFileType::Deleted,
        )
    }

    fn acl_checker() -> Arc<Option<AclChecker>> {
        let checker = AclChecker::new(&Identity::from_groupname("engineers"));
        assert!(checker.do_wait_updated(10000));
        Arc::new(Some(checker))
    }

}
