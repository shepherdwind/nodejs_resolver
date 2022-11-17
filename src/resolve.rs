use crate::{
    description::PkgInfo,
    plugin::{
        AliasFieldPlugin, ExportsFieldPlugin, ImportsFieldPlugin, MainFieldPlugin, MainFilePlugin,
        Plugin,
    },
    Context, EnforceExtension, Info, ResolveResult, Resolver, State, MODULE,
};
use smol_str::SmolStr;
use std::path::{Path, PathBuf};

impl Resolver {
    pub(crate) fn append_ext_for_path(path: &Path, ext: &str) -> PathBuf {
        PathBuf::from(&format!("{}{ext}", path.display()))
    }

    fn resolve_file_with_ext(&self, path: PathBuf, info: Info) -> State {
        for ext in &self.options.extensions {
            let path = Self::append_ext_for_path(&path, ext);
            let is_file = match self.load_entry(&path) {
                Ok(entry) => entry.is_file(),
                Err(err) => return State::Error(err),
            };
            if is_file {
                return State::Success(ResolveResult::Info(info.with_path(path).with_target("")));
            }
        }
        State::Resolving(info)
    }

    #[tracing::instrument]
    pub(crate) fn resolve_as_file(&self, info: Info) -> State {
        let path = info.get_path();
        if matches!(self.options.enforce_extension, EnforceExtension::Enabled) {
            return self.resolve_file_with_ext(path, info);
        }
        let is_file = match self.load_entry(&path) {
            Ok(entry) => entry.is_file(),
            Err(err) => return State::Error(err),
        };
        if is_file {
            State::Success(ResolveResult::Info(info.with_path(path).with_target("")))
        } else {
            self.resolve_file_with_ext(path, info)
        }
    }

    #[tracing::instrument]
    pub(crate) fn resolve_as_dir(&self, info: Info, context: &mut Context) -> State {
        let dir = info.get_path();
        let entry = match self.load_entry(&dir) {
            Ok(entry) => entry,
            Err(err) => return State::Error(err),
        };
        if !entry.is_dir() {
            return State::Failed(info);
        }
        let pkg_info = &entry.pkg_info;
        let info = info.with_path(dir).with_target("");
        if let Some(pkg_info) = pkg_info {
            MainFieldPlugin::new(pkg_info).apply(self, info, context)
        } else {
            State::Resolving(info)
        }
        .then(|info| MainFilePlugin.apply(self, info, context))
    }

    #[tracing::instrument]
    pub(crate) fn resolve_as_modules(&self, info: Info, context: &mut Context) -> State {
        let original_dir = info.path.clone();
        let module_root_path = original_dir.join(MODULE);
        let is_dir = match self.load_entry(&module_root_path) {
            Ok(entry) => entry.is_dir(),
            Err(err) => return State::Error(err),
        };
        let stats = if is_dir {
            let request_module_name = get_module_name_from_request(&info.request.target);
            let module_path = module_root_path.join(&*request_module_name);
            let entry = match self.load_entry(&module_path) {
                Ok(entry) => entry.clone(),
                Err(err) => return State::Error(err),
            };
            let module_path_is_dir = entry.is_dir();
            let is_resolve_self = entry.pkg_info.as_ref().map_or(false, |pkg_info| {
                is_resolve_self(pkg_info, &request_module_name)
            });
            let module_info = Info::from(module_root_path, info.request.clone());
            if !module_path_is_dir && !is_resolve_self {
                let state = self.resolve_as_file(module_info);
                if state.is_finished() {
                    state
                } else {
                    State::Resolving(info)
                }
            } else {
                let state = if let Some(pkg_info) = &entry.pkg_info {
                    dbg!(&pkg_info.dir_path);
                    dbg!(&original_dir);
                    let out_node_modules = pkg_info.dir_path.eq(&original_dir);
                    dbg!(&out_node_modules);

                    if !out_node_modules || is_resolve_self {
                        ExportsFieldPlugin::new(pkg_info).apply(self, module_info, context)
                    } else {
                        State::Resolving(module_info)
                    }
                    .then(|info| ImportsFieldPlugin::new(pkg_info).apply(self, info, context))
                    .then(|info| {
                        let path = info.path.join(&*info.request.target);
                        let info = info.with_path(path).with_target(".");
                        MainFieldPlugin::new(pkg_info).apply(self, info, context)
                    })
                    .then(|info| AliasFieldPlugin::new(pkg_info).apply(self, info, context))
                } else {
                    State::Resolving(module_info)
                }
                .then(|info| self.resolve_as_file(info))
                .then(|info| self.resolve_as_dir(info, context));

                match state {
                    State::Failed(info) => State::Resolving(info),
                    _ => state,
                }
            }
        } else {
            State::Resolving(info)
        }
        .then(|info| {
            if let Some(parent_dir) = original_dir.parent() {
                self._resolve(info.with_path(parent_dir.to_path_buf()), context)
            } else {
                State::Resolving(info)
            }
        });

        match stats {
            State::Resolving(info) => State::Failed(info),
            _ => stats,
        }
    }
}

fn get_module_name_from_request(target: &SmolStr) -> SmolStr {
    let has_namespace_scope = target.starts_with('@');
    let chars = target.chars().enumerate();
    let slash_index_list: Vec<usize> = chars
        .filter(|(_, char)| '/'.eq(char))
        .map(|(index, _)| index)
        .collect();
    if has_namespace_scope {
        slash_index_list.get(1)
    } else {
        slash_index_list.first()
    }
    .map_or(target.clone(), |&index| SmolStr::new(&target[0..index]))
}

fn is_resolve_self(pkg_info: &PkgInfo, request_module_name: &SmolStr) -> bool {
    pkg_info
        .json
        .name
        .as_ref()
        .map(|pkg_name| request_module_name.eq(pkg_name))
        .map_or(false, |ans| ans)
}

#[test]
fn test_get_module_name_from_request() {
    assert_eq!(get_module_name_from_request(&s("a")), s("a"));
    assert_eq!(get_module_name_from_request(&s("a/b")), s("a"));
    assert_eq!(get_module_name_from_request(&s("@a")), s("@a"));
    assert_eq!(get_module_name_from_request(&s("@a/b")), s("@a/b"));
    assert_eq!(get_module_name_from_request(&s("@a/b/c")), s("@a/b"));
    fn s(s: &str) -> SmolStr {
        SmolStr::new(s)
    }
}
