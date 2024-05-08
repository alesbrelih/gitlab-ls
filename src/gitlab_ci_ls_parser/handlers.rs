use std::{collections::HashMap, path::PathBuf, sync::Mutex, time::Instant};

use anyhow::anyhow;
use log::{error, info, warn};
use lsp_server::{Notification, Request};
use lsp_types::{
    request::GotoTypeDefinitionParams, CompletionParams, Diagnostic, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, HoverParams, Position, Url,
};

use crate::gitlab_ci_ls_parser::DiagnosticsNotification;

use super::{
    fs_utils, parser, parser_utils, treesitter, CompletionResult, Component, ComponentInput,
    DefinitionResult, GitlabElement, GitlabInputElement, HoverResult, IncludeInformation,
    LSPCompletion, LSPConfig, LSPLocation, LSPPosition, LSPResult, Range, ReferencesResult,
    RuleReference,
};

#[allow(clippy::module_name_repetitions)]
pub struct LSPHandlers {
    cfg: LSPConfig,
    store: Mutex<HashMap<String, String>>,
    nodes: Mutex<HashMap<String, HashMap<String, String>>>,
    stages: Mutex<HashMap<String, GitlabElement>>,
    variables: Mutex<HashMap<String, GitlabElement>>,
    components: Mutex<HashMap<String, Component>>,
    indexing_in_progress: Mutex<bool>,
    parser: Box<dyn parser::Parser>,
}

impl LSPHandlers {
    pub fn new(cfg: LSPConfig, fs_utils: Box<dyn fs_utils::FSUtils>) -> LSPHandlers {
        let store = Mutex::new(HashMap::new());
        let nodes = Mutex::new(HashMap::new());
        let stages = Mutex::new(HashMap::new());
        let variables = Mutex::new(HashMap::new());
        let components = Mutex::new(HashMap::new());
        let indexing_in_progress = Mutex::new(false);

        let events = LSPHandlers {
            cfg: cfg.clone(),
            store,
            nodes,
            stages,
            variables,
            components,
            indexing_in_progress,
            parser: Box::new(parser::ParserImpl::new(
                cfg.remote_urls,
                cfg.package_map,
                cfg.cache_path,
                Box::new(treesitter::TreesitterImpl::new()),
                fs_utils,
            )),
        };

        if let Err(err) = events.index_workspace(events.cfg.root_dir.as_str()) {
            error!("error indexing workspace; err: {}", err);
        }

        events
    }

    pub fn on_hover(&self, request: Request) -> Option<LSPResult> {
        let params = serde_json::from_value::<HoverParams>(request.params).ok()?;

        let store = self.store.lock().unwrap();
        let nodes = self.nodes.lock().unwrap();

        let uri = &params.text_document_position_params.text_document.uri;
        let document = store.get::<String>(&uri.to_string())?;

        let position = params.text_document_position_params.position;
        let line = document.lines().nth(position.line as usize)?;

        let word = parser_utils::ParserUtils::extract_word(line, position.character as usize)?
            .trim_end_matches(':');

        match self.parser.get_position_type(document, position) {
            parser::PositionType::Extend | parser::PositionType::RootNode => {
                for (document_uri, node) in nodes.iter() {
                    for (key, content) in node {
                        if key.eq(word) {
                            let cnt = match self.parser.get_full_definition(
                                GitlabElement {
                                    key: key.clone(),
                                    content: Some(content.to_string()),
                                    uri: document_uri.to_string(),
                                    ..Default::default()
                                },
                                &store,
                            ) {
                                Ok(c) => c,
                                Err(err) => return Some(LSPResult::Error(err)),
                            };

                            return Some(LSPResult::Hover(HoverResult {
                                id: request.id,
                                content: format!("```yaml\n{cnt}\n```"),
                            }));
                        }
                    }
                }

                None
            }
            parser::PositionType::RuleReference(RuleReference { node }) => {
                for (document_uri, n) in nodes.iter() {
                    for (key, content) in n {
                        if key.eq(&node) {
                            let cnt = match self.parser.get_full_definition(
                                GitlabElement {
                                    key: key.clone(),
                                    content: Some(content.to_string()),
                                    uri: document_uri.to_string(),
                                    ..Default::default()
                                },
                                &store,
                            ) {
                                Ok(c) => c,
                                Err(err) => return Some(LSPResult::Error(err)),
                            };

                            return Some(LSPResult::Hover(HoverResult {
                                id: request.id,
                                content: format!("```yaml\n{cnt}\n```"),
                            }));
                        }
                    }
                }

                None
            }
            _ => None,
        }
    }

    pub fn on_change(&self, notification: Notification) -> Option<LSPResult> {
        let start = Instant::now();
        let params =
            serde_json::from_value::<DidChangeTextDocumentParams>(notification.params).ok()?;

        if params.content_changes.len() != 1 {
            return None;
        }

        // TODO: nodes

        let mut store = self.store.lock().unwrap();
        let mut all_nodes = self.nodes.lock().unwrap();
        // reset previous
        all_nodes.insert(params.text_document.uri.to_string(), HashMap::new());

        let mut all_variables = self.variables.lock().unwrap();

        let mut all_components = self.components.lock().unwrap();

        if let Some(results) = self.parser.parse_contents(
            &params.text_document.uri,
            &params.content_changes.first()?.text,
            false,
        ) {
            for file in results.files {
                store.insert(file.path, file.content);
            }

            for node in results.nodes {
                info!("found node: {:?}", &node);
                all_nodes
                    .entry(node.uri)
                    .or_default()
                    .insert(node.key, node.content?);
            }

            if !results.stages.is_empty() {
                let mut all_stages = self.stages.lock().unwrap();
                all_stages.clear();

                for stage in results.stages {
                    info!("found stage: {:?}", &stage);
                    all_stages.insert(stage.key.clone(), stage);
                }
            }

            // should be per file...
            // TODO: clear correct variables
            for variable in results.variables {
                info!("found variable: {:?}", &variable);
                all_variables.insert(variable.key.clone(), variable);
            }

            for component in results.components {
                info!("found component: {:?}", &component);
                all_components.insert(component.uri.clone(), component);
            }
        }

        info!("ONCHANGE ELAPSED: {:?}", start.elapsed());

        None
    }

    pub fn on_open(&self, notification: Notification) -> Option<LSPResult> {
        let in_progress = self.indexing_in_progress.lock().unwrap();
        drop(in_progress);

        let params =
            serde_json::from_value::<DidOpenTextDocumentParams>(notification.params).ok()?;

        let mut store = self.store.lock().unwrap();
        let mut all_nodes = self.nodes.lock().unwrap();
        let mut all_stages = self.stages.lock().unwrap();

        if let Some(results) =
            self.parser
                .parse_contents(&params.text_document.uri, &params.text_document.text, true)
        {
            for file in results.files {
                store.insert(file.path, file.content);
            }

            for node in results.nodes {
                info!("found node: {:?}", &node);

                all_nodes
                    .entry(node.uri)
                    .or_default()
                    .insert(node.key, node.content?);
            }

            for stage in results.stages {
                info!("found stage: {:?}", &stage);
                all_stages.insert(stage.key.clone(), stage);
            }
        }

        info!("finished searching");

        // releasing lock because generate diagnostics grabs it
        // and is used in two places
        drop(store);
        drop(all_nodes);
        drop(all_stages);

        self.generate_diagnostics(params.text_document.uri)
    }

    #[allow(clippy::too_many_lines)]
    pub fn on_definition(&self, request: Request) -> Option<LSPResult> {
        let params = serde_json::from_value::<GotoTypeDefinitionParams>(request.params).ok()?;

        let store = self.store.lock().unwrap();
        let store = &*store;
        let document_uri = params.text_document_position_params.text_document.uri;
        let document = store.get::<String>(&document_uri.to_string())?;
        let position = params.text_document_position_params.position;
        let stages = self.stages.lock().unwrap();

        let mut locations: Vec<LSPLocation> = vec![];

        match self.parser.get_position_type(document, position) {
            parser::PositionType::RootNode | parser::PositionType::Extend => {
                let line = document.lines().nth(position.line as usize)?;
                let word =
                    parser_utils::ParserUtils::extract_word(line, position.character as usize)?
                        .trim_end_matches(':');

                for (uri, content) in store {
                    if let Some(element) = self.parser.get_root_node(uri, content, word) {
                        if document_uri.as_str().ends_with(uri)
                            && line.eq(&format!("{}:", element.key.as_str()))
                        {
                            continue;
                        }

                        locations.push(LSPLocation {
                            uri: uri.clone(),
                            range: element.range,
                        });
                    }
                }
            }
            parser::PositionType::Include(info) => {
                if let Some(include) = self.on_definition_include(info, store) {
                    locations.push(include);
                }
            }
            parser::PositionType::Needs(node) => {
                for (uri, content) in store {
                    if let Some(element) = self.parser.get_root_node(
                        uri,
                        content,
                        parser_utils::ParserUtils::strip_quotes(node.name.as_str()),
                    ) {
                        locations.push(LSPLocation {
                            uri: uri.clone(),
                            range: element.range,
                        });
                    }
                }
            }
            parser::PositionType::Stage => {
                let line = document.lines().nth(position.line as usize)?;
                let word =
                    parser_utils::ParserUtils::extract_word(line, position.character as usize)?;

                if let Some(el) = stages.get(word) {
                    locations.push(LSPLocation {
                        uri: el.uri.clone(),
                        range: el.range.clone(),
                    });
                }
            }
            parser::PositionType::Variable => {
                let line = document.lines().nth(position.line as usize)?;
                let word =
                    parser_utils::ParserUtils::extract_variable(line, position.character as usize)?;

                let variable_locations = self.parser.get_variable_definitions(
                    word,
                    document_uri.as_str(),
                    position,
                    store,
                )?;

                for location in variable_locations {
                    locations.push(LSPLocation {
                        uri: location.uri,
                        range: location.range,
                    });
                }
                let mut root = self
                    .variables
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(name, _)| name.starts_with(word))
                    .map(|(_, el)| LSPLocation {
                        uri: el.uri.clone(),
                        range: el.range.clone(),
                    })
                    .collect::<Vec<LSPLocation>>();

                locations.append(&mut root);
            }
            parser::PositionType::RuleReference(RuleReference { node }) => {
                for (uri, content) in store {
                    if let Some(element) = self.parser.get_root_node(uri, content, &node) {
                        locations.push(LSPLocation {
                            uri: uri.clone(),
                            range: element.range,
                        });
                    }
                }
            }
            parser::PositionType::None => {
                error!("invalid position type for goto def");
                return None;
            }
        };

        Some(LSPResult::Definition(DefinitionResult {
            id: request.id,
            locations,
        }))
    }

    #[allow(clippy::too_many_lines)]
    fn on_definition_include(
        &self,
        info: IncludeInformation,
        store: &HashMap<String, String>,
    ) -> Option<LSPLocation> {
        match info {
            IncludeInformation {
                local: Some(local),
                remote: None,
                remote_url: None,
                basic: None,
                component: None,
            } => {
                let local = parser_utils::ParserUtils::strip_quotes(&local.path);

                LSPHandlers::on_definition_local(local, store)
            }
            IncludeInformation {
                local: None,
                remote: Some(remote),
                remote_url: None,
                basic: None,
                component: None,
            } => {
                let file = remote.file?;
                let file = parser_utils::ParserUtils::strip_quotes(&file).trim_start_matches('/');

                let path = format!("{}/{}/{}", remote.project?, remote.reference?, file);

                store
                    .keys()
                    .find(|uri| uri.ends_with(&path))
                    .map(|uri| LSPLocation {
                        uri: uri.clone(),
                        range: Range {
                            start: LSPPosition {
                                line: 0,
                                character: 0,
                            },
                            end: LSPPosition {
                                line: 0,
                                character: 0,
                            },
                        },
                    })
            }
            IncludeInformation {
                local: None,
                remote: None,
                remote_url: None,
                basic: None,
                component: Some(component),
            } => {
                let component_uri = component
                    .uri
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();

                self.components
                    .lock()
                    .unwrap()
                    .values()
                    .find(|c| c.uri == component_uri)
                    .map(|c| LSPLocation {
                        uri: c.local_path.clone(),
                        range: Range {
                            start: LSPPosition {
                                line: 0,
                                character: 0,
                            },
                            end: LSPPosition {
                                line: 0,
                                character: 0,
                            },
                        },
                    })
            }
            IncludeInformation {
                local: None,
                remote: None,
                remote_url: Some(remote_url),
                basic: None,
                component: None,
            } => {
                let remote_url = parser_utils::ParserUtils::strip_quotes(remote_url.path.as_str());
                LSPHandlers::on_definition_remote(remote_url, store)
            }
            IncludeInformation {
                local: None,
                remote: None,
                remote_url: None,
                basic: Some(basic_url),
                component: None,
            } => {
                let url = parser_utils::ParserUtils::strip_quotes(&basic_url.path);
                if let Ok(url) = Url::parse(url) {
                    LSPHandlers::on_definition_remote(url.as_str(), store)
                } else {
                    LSPHandlers::on_definition_local(url, store)
                }
            }
            _ => None,
        }
    }

    pub fn on_definition_local(
        local_url: &str,
        store: &HashMap<String, String>,
    ) -> Option<LSPLocation> {
        let local_url = local_url.trim_start_matches('.');

        store
            .keys()
            .find(|uri| uri.ends_with(local_url))
            .map(|uri| LSPLocation {
                uri: uri.clone(),
                range: Range {
                    start: LSPPosition {
                        line: 0,
                        character: 0,
                    },
                    end: LSPPosition {
                        line: 0,
                        character: 0,
                    },
                },
            })
    }

    pub fn on_definition_remote(
        remote_url: &str,
        store: &HashMap<String, String>,
    ) -> Option<LSPLocation> {
        let path_hash = parser_utils::ParserUtils::remote_path_to_hash(remote_url);

        store
            .keys()
            .find(|uri| uri.ends_with(format!("_{path_hash}.yaml").as_str()))
            .map(|uri| LSPLocation {
                uri: uri.clone(),
                range: Range {
                    start: LSPPosition {
                        line: 0,
                        character: 0,
                    },
                    end: LSPPosition {
                        line: 0,
                        character: 0,
                    },
                },
            })
    }

    pub fn on_completion(&self, request: Request) -> Option<LSPResult> {
        let start = Instant::now();
        let params: CompletionParams = serde_json::from_value(request.params).ok()?;

        let store = self.store.lock().unwrap();
        let document_uri = params.text_document_position.text_document.uri;
        let document = store.get::<String>(&document_uri.clone().into())?;

        let position = params.text_document_position.position;
        let line = document.lines().nth(position.line as usize)?;

        let items = match self.parser.get_position_type(document, position) {
            parser::PositionType::Stage => self.on_completion_stages(line, position).ok()?,
            parser::PositionType::Extend => self.on_completion_extends(line, position).ok()?,
            parser::PositionType::Variable => self.on_completion_variables(line, position).ok()?,
            parser::PositionType::Needs(_) => self.on_completion_needs(line, position).ok()?,
            parser::PositionType::Include(IncludeInformation {
                remote: None,
                remote_url: None,
                local: None,
                basic: None,
                component: Some(component),
            }) => self
                .on_completion_component(line, position, &component)
                .ok()?,
            parser::PositionType::RuleReference(_) => {
                self.on_completion_rule_reference(line, position).ok()?
            }
            _ => return None,
        };

        info!("AUTOCOMPLETE ELAPSED: {:?}", start.elapsed());

        Some(LSPResult::Completion(CompletionResult {
            id: request.id,
            list: items,
        }))
    }

    fn on_completion_stages(
        &self,
        line: &str,
        position: Position,
    ) -> anyhow::Result<Vec<LSPCompletion>> {
        let stages = self
            .stages
            .lock()
            .map_err(|e| anyhow::anyhow!("failed to lock stages: {}", e))?;

        let word = parser_utils::ParserUtils::word_before_cursor(
            line,
            position.character as usize,
            |c: char| c.is_whitespace(),
        );
        let after =
            parser_utils::ParserUtils::word_after_cursor(line, position.character as usize, |c| {
                c.is_whitespace()
            });

        let items = stages
            .keys()
            .filter(|stage| stage.contains(word))
            .flat_map(|stage| -> anyhow::Result<LSPCompletion> {
                Ok(LSPCompletion {
                    label: stage.clone(),
                    details: None,
                    location: LSPLocation {
                        range: Range {
                            start: LSPPosition {
                                line: position.line,
                                character: position.character - u32::try_from(word.len())?,
                            },
                            end: LSPPosition {
                                line: position.line,
                                character: position.character + u32::try_from(after.len())?,
                            },
                        },
                        ..Default::default()
                    },
                })
            })
            .collect();

        Ok(items)
    }
    fn on_completion_extends(
        &self,
        line: &str,
        position: Position,
    ) -> anyhow::Result<Vec<LSPCompletion>> {
        let nodes = self
            .nodes
            .lock()
            .map_err(|e| anyhow!("failed to lock nodes: {}", e))?;

        let word = parser_utils::ParserUtils::word_before_cursor(
            line,
            position.character as usize,
            |c: char| c.is_whitespace(),
        );

        let after =
            parser_utils::ParserUtils::word_after_cursor(line, position.character as usize, |c| {
                c.is_whitespace()
            });

        let items = nodes
            .values()
            .flat_map(|n| n.iter())
            .filter(|(node_key, _)| node_key.starts_with('.') && node_key.contains(word))
            .flat_map(
                |(node_key, node_description)| -> anyhow::Result<LSPCompletion> {
                    Ok(LSPCompletion {
                        label: node_key.to_string(),
                        details: Some(format!("```yaml\r\n{node_description}\r\n```")),
                        location: LSPLocation {
                            range: Range {
                                start: LSPPosition {
                                    line: position.line,
                                    character: position
                                        .character
                                        .saturating_sub(u32::try_from(word.len())?),
                                },
                                end: LSPPosition {
                                    line: position.line,
                                    character: position.character + u32::try_from(after.len())?,
                                },
                            },
                            ..Default::default()
                        },
                    })
                },
            )
            .collect();

        Ok(items)
    }

    fn on_completion_variables(
        &self,
        line: &str,
        position: Position,
    ) -> anyhow::Result<Vec<LSPCompletion>> {
        let variables = self
            .variables
            .lock()
            .map_err(|e| anyhow!("failed to lock variables: {}", e))?;

        let word = parser_utils::ParserUtils::word_before_cursor(
            line,
            position.character as usize,
            |c: char| c == '$',
        );

        let after =
            parser_utils::ParserUtils::word_after_cursor(line, position.character as usize, |c| {
                c.is_whitespace()
            });

        let items = variables
            .keys()
            .filter(|v| v.starts_with(word))
            .flat_map(|v| -> anyhow::Result<LSPCompletion> {
                Ok(LSPCompletion {
                    label: v.clone(),
                    details: None,
                    location: LSPLocation {
                        range: Range {
                            start: LSPPosition {
                                line: position.line,
                                character: position.character - u32::try_from(word.len())?,
                            },
                            end: LSPPosition {
                                line: position.line,
                                character: position.character + u32::try_from(after.len())?,
                            },
                        },
                        ..Default::default()
                    },
                })
            })
            .collect();

        Ok(items)
    }

    fn on_completion_rule_reference(
        &self,
        line: &str,
        position: Position,
    ) -> anyhow::Result<Vec<LSPCompletion>> {
        let nodes = self
            .nodes
            .lock()
            .map_err(|err| anyhow!("failed to lock nodes: {}", err))?;

        let word = parser_utils::ParserUtils::word_before_cursor(
            line,
            position.character as usize,
            |c: char| c == '\'' || c == '"',
        );

        let after =
            parser_utils::ParserUtils::word_after_cursor(line, position.character as usize, |c| {
                c == '\'' || c == '"'
            });

        let items = nodes
            .values()
            .flat_map(|needs| needs.iter())
            .filter(|(node_key, _)| node_key.contains(word))
            .flat_map(
                |(node_key, node_description)| -> anyhow::Result<LSPCompletion> {
                    Ok(LSPCompletion {
                        label: node_key.clone(),
                        details: Some(format!("```yaml\r\n{node_description}\r\n```")),
                        location: LSPLocation {
                            range: Range {
                                start: LSPPosition {
                                    line: position.line,
                                    character: position.character - u32::try_from(word.len())?,
                                },
                                end: LSPPosition {
                                    line: position.line,
                                    character: position.character + u32::try_from(after.len())?,
                                },
                            },
                            ..Default::default()
                        },
                    })
                },
            )
            .collect();

        Ok(items)
    }

    fn on_completion_needs(
        &self,
        line: &str,
        position: Position,
    ) -> anyhow::Result<Vec<LSPCompletion>> {
        let nodes = self
            .nodes
            .lock()
            .map_err(|err| anyhow!("failed to lock nodes: {}", err))?;
        let word = parser_utils::ParserUtils::word_before_cursor(
            line,
            position.character as usize,
            |c: char| c.is_whitespace(),
        );
        let after =
            parser_utils::ParserUtils::word_after_cursor(line, position.character as usize, |c| {
                c.is_whitespace()
            });

        let items = nodes
            .values()
            .flat_map(|needs| needs.iter())
            .filter(|(node_key, _)| !node_key.starts_with('.') && node_key.contains(word))
            .flat_map(
                |(node_key, node_description)| -> anyhow::Result<LSPCompletion> {
                    Ok(LSPCompletion {
                        label: node_key.clone(),
                        details: Some(format!("```yaml\r\n{node_description}\r\n```")),
                        location: LSPLocation {
                            range: Range {
                                start: LSPPosition {
                                    line: position.line,
                                    character: position.character - u32::try_from(word.len())?,
                                },
                                end: LSPPosition {
                                    line: position.line,
                                    character: position.character + u32::try_from(after.len())?,
                                },
                            },
                            ..Default::default()
                        },
                    })
                },
            )
            .collect();

        Ok(items)
    }

    fn index_workspace(&self, root_dir: &str) -> anyhow::Result<()> {
        let mut in_progress = self.indexing_in_progress.lock().unwrap();
        *in_progress = true;

        let start = Instant::now();

        let mut store = self.store.lock().unwrap();
        let mut all_nodes = self.nodes.lock().unwrap();
        let mut all_stages = self.stages.lock().unwrap();
        let mut all_variables = self.variables.lock().unwrap();
        let mut all_components = self.components.lock().unwrap();

        info!("importing files from base");
        let base_uri = format!("{}base", self.cfg.cache_path);
        let base_uri_path = Url::parse(format!("file://{base_uri}/").as_str())?;
        for dir in std::fs::read_dir(&base_uri)?.flatten() {
            let file_uri = base_uri_path.join(dir.file_name().to_str().unwrap())?;
            let file_content = std::fs::read_to_string(dir.path())?;

            if let Some(results) = self.parser.parse_contents(&file_uri, &file_content, false) {
                for file in results.files {
                    info!("found file: {:?}", &file);
                    store.insert(file.path, file.content);
                }

                for node in results.nodes {
                    info!("found node: {:?}", &node);
                    let content = node.content.unwrap_or(String::new());

                    all_nodes
                        .entry(node.uri)
                        .or_default()
                        .insert(node.key, content);
                }

                for stage in results.stages {
                    info!("found stage: {:?}", &stage);
                    all_stages.insert(stage.key.clone(), stage);
                }

                for variable in results.variables {
                    info!("found variable: {:?}", &variable);
                    all_variables.insert(variable.key.clone(), variable);
                }

                for component in results.components {
                    info!("found component: {:?}", &component);
                    all_components.insert(component.uri.clone(), component);
                }
            }
        }

        info!("importing from root file");
        let mut uri = Url::parse(format!("file://{root_dir}/").as_str())?;
        info!("uri: {}", &uri);

        let list = std::fs::read_dir(root_dir)?;
        let mut root_file: Option<PathBuf> = None;

        for item in list.flatten() {
            if item.file_name() == ".gitlab-ci.yaml" || item.file_name() == ".gitlab-ci.yml" {
                root_file = Some(item.path());
                break;
            }
        }

        let root_file_content = match root_file {
            Some(root_file) => {
                let file_name = root_file.file_name().unwrap().to_str().unwrap();
                uri = uri.join(file_name)?;

                std::fs::read_to_string(root_file)?
            }
            _ => {
                return Err(anyhow::anyhow!("root file missing"));
            }
        };

        info!("URI: {}", &uri);
        if let Some(results) = self.parser.parse_contents(&uri, &root_file_content, true) {
            for file in results.files {
                info!("found file: {:?}", &file);
                store.insert(file.path, file.content);
            }

            for node in results.nodes {
                info!("found node: {:?}", &node);
                let content = node.content.unwrap_or(String::new());

                all_nodes
                    .entry(node.uri)
                    .or_default()
                    .insert(node.key, content);
            }

            for stage in results.stages {
                info!("found stage: {:?}", &stage);
                all_stages.insert(stage.key.clone(), stage);
            }

            for variable in results.variables {
                info!("found variable: {:?}", &variable);
                all_variables.insert(variable.key.clone(), variable);
            }

            for component in results.components {
                info!("found component: {:?}", &component);
                all_components.insert(component.uri.clone(), component);
            }
        }

        error!("INDEX WORKSPACE ELAPSED: {:?}", start.elapsed());

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn generate_diagnostics(&self, document_uri: lsp_types::Url) -> Option<LSPResult> {
        let start = Instant::now();
        let store = self.store.lock().unwrap();
        let all_nodes = self.nodes.lock().unwrap();

        let content: String = store.get(&document_uri.to_string())?.to_string();

        let extends = self
            .parser
            .get_all_extends(document_uri.to_string(), content.as_str(), None);

        let mut diagnostics: Vec<Diagnostic> = vec![];

        'extend: for extend in extends {
            if extend.uri == document_uri.to_string() {
                for (_, root_nodes) in all_nodes.iter() {
                    if root_nodes.get(&extend.key).is_some() {
                        continue 'extend;
                    }
                }

                diagnostics.push(Diagnostic::new_simple(
                    lsp_types::Range {
                        start: lsp_types::Position {
                            line: extend.range.start.line,
                            character: extend.range.start.character,
                        },
                        end: lsp_types::Position {
                            line: extend.range.end.line,
                            character: extend.range.end.character,
                        },
                    },
                    format!("Rule: {} does not exist.", extend.key),
                ));
            }
        }

        let stages = self
            .parser
            .get_all_stages(document_uri.as_ref(), content.as_str(), None);

        let all_stages = self.stages.lock().unwrap();
        for stage in stages {
            if all_stages.get(&stage.key).is_none() {
                diagnostics.push(Diagnostic::new_simple(
                    lsp_types::Range {
                        start: lsp_types::Position {
                            line: stage.range.start.line,
                            character: stage.range.start.character,
                        },
                        end: lsp_types::Position {
                            line: stage.range.end.line,
                            character: stage.range.end.character,
                        },
                    },
                    format!("Stage: {} does not exist.", stage.key),
                ));
            }
        }

        let needs = self
            .parser
            .get_all_job_needs(document_uri.to_string(), content.as_str(), None);

        'needs: for need in needs {
            for (_, node) in all_nodes.iter() {
                if node.get(need.key.as_str()).is_some() {
                    continue 'needs;
                }
            }
            diagnostics.push(Diagnostic::new_simple(
                lsp_types::Range {
                    start: lsp_types::Position {
                        line: need.range.start.line,
                        character: need.range.start.character,
                    },
                    end: lsp_types::Position {
                        line: need.range.end.line,
                        character: need.range.end.character,
                    },
                },
                format!("Job: {} does not exist.", need.key),
            ));
        }

        let components = self
            .parser
            .get_all_components(document_uri.as_ref(), content.as_str());

        let all_components = self.components.lock().unwrap();
        for component in components {
            if let Some(spec) = all_components.get(&component.key) {
                component.inputs.iter().for_each(|i| {
                    // check invalid ones -> those that aren't defined in spec
                    let spec_definition = &spec.inputs.iter().find(|si| si.key == i.key);

                    if let Some(spec_definition) = spec_definition {
                        generate_component_diagnostics_from_spec(
                            i,
                            spec_definition,
                            &mut diagnostics,
                        );
                    } else {
                        // wasn't found in spec -> invalid key
                        diagnostics.push(Diagnostic::new_simple(
                            lsp_types::Range {
                                start: lsp_types::Position {
                                    line: i.range.start.line,
                                    character: i.range.start.character,
                                },
                                end: lsp_types::Position {
                                    line: i.range.end.line,
                                    character: i.range.end.character,
                                },
                            },
                            format!(
                                "Invalid input key. Key needs to be one of: '{}'.",
                                spec.inputs
                                    .iter()
                                    .map(|i| i.key.clone())
                                    .collect::<Vec<String>>()
                                    .join(", ")
                            ),
                        ));
                    }
                });
            }
        }

        info!("DIAGNOSTICS ELAPSED: {:?}", start.elapsed());
        Some(LSPResult::Diagnostics(DiagnosticsNotification {
            uri: document_uri,
            diagnostics,
        }))
    }

    pub fn on_save(&self, notification: Notification) -> Option<LSPResult> {
        let params =
            serde_json::from_value::<DidSaveTextDocumentParams>(notification.params).ok()?;

        self.generate_diagnostics(params.text_document.uri)
    }

    pub fn on_references(&self, request: Request) -> Option<LSPResult> {
        let start = Instant::now();

        let params = serde_json::from_value::<lsp_types::ReferenceParams>(request.params).ok()?;

        let store = self.store.lock().unwrap();
        let document_uri = &params.text_document_position.text_document.uri;
        let document = store.get::<String>(&document_uri.to_string())?;

        let position = params.text_document_position.position;
        let line = document.lines().nth(position.line as usize)?;

        let position_type = self.parser.get_position_type(document, position);
        let mut references: Vec<GitlabElement> = vec![];

        match position_type {
            parser::PositionType::Extend => {
                let word =
                    parser_utils::ParserUtils::extract_word(line, position.character as usize)?;

                for (uri, content) in store.iter() {
                    let mut extends =
                        self.parser
                            .get_all_extends(uri.to_string(), content.as_str(), Some(word));
                    references.append(&mut extends);
                }
            }
            parser::PositionType::RootNode => {
                let word =
                    parser_utils::ParserUtils::extract_word(line, position.character as usize)?
                        .trim_end_matches(':');

                // currently support only those that are extends
                if word.starts_with('.') {
                    for (uri, content) in store.iter() {
                        let mut extends = self.parser.get_all_extends(
                            uri.to_string(),
                            content.as_str(),
                            Some(word),
                        );
                        references.append(&mut extends);
                    }
                } else {
                    for (uri, content) in store.iter() {
                        let mut extends = self.parser.get_all_job_needs(
                            uri.to_string(),
                            content.as_str(),
                            Some(word),
                        );
                        references.append(&mut extends);
                    }
                }
            }
            parser::PositionType::Stage => {
                let word =
                    parser_utils::ParserUtils::extract_word(line, position.character as usize);

                for (uri, content) in store.iter() {
                    let mut stages = self.parser.get_all_stages(uri, content.as_str(), word);
                    references.append(&mut stages);
                }
            }
            _ => {}
        }

        info!("REFERENCES ELAPSED: {:?}", start.elapsed());

        Some(LSPResult::References(ReferencesResult {
            id: request.id,
            locations: references,
        }))
    }

    #[allow(clippy::unnecessary_wraps, clippy::too_many_lines)]
    fn on_completion_component(
        &self,
        line: &str,
        position: Position,
        component: &Component,
    ) -> anyhow::Result<Vec<LSPCompletion>> {
        if component.inputs.iter().any(|i| i.hovered) {
            let word = parser_utils::ParserUtils::word_before_cursor(
                line,
                position.character as usize,
                |c: char| c.is_whitespace(),
            );

            let after = parser_utils::ParserUtils::word_after_cursor(
                line,
                position.character as usize,
                |c| c.is_whitespace() || c == ':',
            );

            let components_store = self.components.lock().unwrap();
            let Some(component_spec) = components_store.get(&component.uri) else {
                warn!(
                    "could not find component spec; indexing went wrong!; searching for {}",
                    component.uri
                );

                return Ok(vec![]);
            };

            // filter out those that were already used
            let valid_input_autocompletes: Vec<super::ComponentInput> = component_spec
                .inputs
                .iter()
                .filter(|&i| !component.inputs.iter().any(|ci| ci.key == i.key))
                .cloned() // Clone each element to get an owned version
                .collect();

            let items = valid_input_autocompletes
                .into_iter()
                .filter(|i| i.key.starts_with(word))
                .flat_map(|i| -> anyhow::Result<LSPCompletion> {
                    Ok(LSPCompletion {
                        label: i.key.clone(),
                        details: Some(i.autocomplete_details()),
                        location: LSPLocation {
                            range: Range {
                                start: LSPPosition {
                                    line: position.line,
                                    character: position.character - u32::try_from(word.len())?,
                                },
                                end: LSPPosition {
                                    line: position.line,
                                    character: position.character + u32::try_from(after.len())?,
                                },
                            },
                            ..Default::default()
                        },
                    })
                })
                .collect();

            return Ok(items);
        } else if let Some(hovered_input) = component.inputs.iter().find(|i| i.value_plain.hovered)
        {
            let word = parser_utils::ParserUtils::word_before_cursor(
                line,
                position.character as usize,
                |c| c.is_whitespace() || c == ':',
            );

            let after = parser_utils::ParserUtils::word_after_cursor(
                line,
                position.character as usize,
                |c: char| c.is_whitespace(),
            );

            let components_store = self.components.lock().unwrap();
            let Some(component_spec) = components_store.get(&component.uri) else {
                warn!(
                    "could not find component spec; indexing went wrong!; searching for {}",
                    component.uri
                );

                return Ok(vec![]);
            };

            if let Some(input_spec) = component_spec
                .inputs
                .iter()
                .find(|i| i.key == hovered_input.key)
            {
                if let Some(options) = &input_spec.options {
                    let items = options
                        .iter()
                        .filter(|option| option.starts_with(word))
                        .flat_map(|option| -> anyhow::Result<LSPCompletion> {
                            Ok(LSPCompletion {
                                label: option.to_string(),
                                details: None,
                                location: LSPLocation {
                                    range: Range {
                                        start: LSPPosition {
                                            line: position.line,
                                            character: position.character
                                                - u32::try_from(word.len())?,
                                        },
                                        end: LSPPosition {
                                            line: position.line,
                                            character: position.character
                                                + u32::try_from(after.len())?,
                                        },
                                    },
                                    ..Default::default()
                                },
                            })
                        })
                        .collect();

                    return Ok(items);
                }
            }
        }

        Ok(vec![])
    }
}

fn generate_component_diagnostics_from_spec(
    i: &GitlabInputElement,
    spec_definition: &ComponentInput,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if let Some(options) = &spec_definition.options {
        if let Some(input_value_element) = &i.value_plain {
            if let Some(input_value) = &input_value_element.content {
                if !options.contains(input_value) {
                    diagnostics.push(Diagnostic::new_simple(
                        lsp_types::Range {
                            start: lsp_types::Position {
                                line: input_value_element.range.start.line,
                                character: input_value_element.range.start.character,
                            },
                            end: lsp_types::Position {
                                line: input_value_element.range.end.line,
                                character: input_value_element.range.end.character,
                            },
                        },
                        format!(
                            "Invalid input value. Value needs to be one of: '{}'.",
                            options.join(", ")
                        ),
                    ));
                }
            }
        } else {
            diagnostics.push(Diagnostic::new_simple(
                lsp_types::Range {
                    start: lsp_types::Position {
                        line: i.range.start.line,
                        character: i.range.start.character,
                    },
                    end: lsp_types::Position {
                        line: i.range.end.line,
                        character: i.range.end.character,
                    },
                },
                format!(
                    "No value. Value needs to be one of: '{}'.",
                    options.join(", ")
                ),
            ));
        }
    }
}
