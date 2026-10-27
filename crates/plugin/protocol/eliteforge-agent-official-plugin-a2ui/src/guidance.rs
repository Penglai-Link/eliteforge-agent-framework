//! Capability-gated instructions and progressive skill resources.
use super::*;

pub const A2UI_PACK_ID: &str = "framework.a2ui";
pub const A2UI_SKILL_NAME: &str = "a2ui-interface";
const SKILL: &str = include_str!("../skills/a2ui-interface/SKILL.md");
const EXAMPLES: &str = include_str!("../skills/a2ui-interface/references/examples.json");
const COMPONENTS: &str = include_str!("../skills/a2ui-interface/references/components.md");
const DEFAULT_A2UI_GUIDANCE: &str = include_str!("../prompts/a2ui-guidance.txt");

/// Installs A2UI and requests the Framework's single aggregate Skills runtime.
/// Omit this pack entirely when a host does not support A2UI.
pub struct A2uiPack {
    plugin: Arc<dyn Plugin>,
}
impl A2uiPack {
    pub fn new() -> Result<Self, AgentError> {
        Ok(Self {
            plugin: Arc::new(A2uiPlugin::new()?),
        })
    }
}
impl CapabilityPack for A2uiPack {
    fn id(&self) -> &str {
        A2UI_PACK_ID
    }
    fn configuration(&self) -> &CapabilityConfig {
        &()
    }
    fn requires_skills_runtime(&self) -> bool {
        true
    }
    fn registry_skill_sources(&self) -> Vec<CapabilitySkillSource> {
        const FILES: &[CapabilitySkillFile] = &[
            CapabilitySkillFile { path: "a2ui-interface/SKILL.md", contents: SKILL.as_bytes() },
            CapabilitySkillFile { path: "a2ui-interface/references/components.md", contents: COMPONENTS.as_bytes() },
            CapabilitySkillFile { path: "a2ui-interface/references/examples.json", contents: EXAMPLES.as_bytes() },
        ];
        vec![CapabilitySkillSource::embedded(EmbeddedCapabilitySkills::new(FILES, "a2ui-skill-v1"))]
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        vec![self.plugin.clone()]
    }
}

pub fn negotiated(extensions: &Extensions) -> bool {
    extensions
        .get::<SkillAccess>()
        .is_none_or(|access| access.allows(A2UI_SKILL_NAME))
        && extensions.get::<A2uiRunOptions>().is_some_and(|options| {
            options.capabilities.version == VERSION
                && options
                    .capabilities
                    .catalog_ids
                    .iter()
                    .any(|id| [AGENT_CATALOG_ID, MINIMAL_CATALOG_ID].contains(&id.as_str()))
        })
}
pub fn is_a2ui_tool(descriptor: &ToolDescriptor) -> bool {
    matches!(
        (descriptor.id.as_str(), descriptor.call_name.as_str()),
        ("a2ui.publish", "a2ui_publish")
            | ("a2ui.request_input", "a2ui_request_input")
    )
}

#[derive(Clone)]
pub(crate) struct Guidance;
#[async_trait]
impl ContextContributor for Guidance {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        "a2ui.guidance"
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        let messages = if negotiated(&input.run.extensions) {
            vec![Message::system(ManagedDeveloperPrompts::resolve(
                input.run,
                "a2ui.guidance",
                DEFAULT_A2UI_GUIDANCE.trim(),
            )?)]
        } else {
            vec![]
        };
        Ok(ContextBlock::instructions(self.block_id(), messages))
    }
}
fn summary() -> SkillSummary {
    SkillSummary { name:A2UI_SKILL_NAME.into(),description:"Create or update interactive chat cards, structured input forms, reports, and reusable query controls with the negotiated A2UI catalog.".into(),source:"framework.a2ui".into(),location:"framework://a2ui/skills/a2ui-interface".into(),license:None,compatibility:Some("A2UI v0.9.1; negotiated EliteForge catalog".into()),metadata:BTreeMap::new(),allowed_tools:None }
}
fn catalogs(run: &RunContext) -> Vec<(&'static str, Value)> {
    let options = run.extensions.get::<A2uiRunOptions>().unwrap();
    options
        .capabilities
        .catalog_ids
        .iter()
        .filter_map(|id| match id.as_str() {
            AGENT_CATALOG_ID => Some((
                "agent",
                serde_json::from_str(include_str!("schemas/agent_catalog.json")).unwrap(),
            )),
            MINIMAL_CATALOG_ID => Some((
                "minimal",
                serde_json::from_str(include_str!("schemas/catalog.json")).unwrap(),
            )),
            _ => None,
        })
        .collect()
}
fn resource_paths(run: &RunContext) -> Vec<String> {
    let mut paths = vec![
        "references/components.md".into(),
        "references/examples.json".into(),
        "catalog.json".into(),
        "protocol.json".into(),
        "common-types.json".into(),
    ];
    for (alias, catalog) in catalogs(run) {
        for name in catalog["components"].as_object().unwrap().keys() {
            paths.push(format!("components/{alias}/{name}.json"));
        }
    }
    paths
}
fn resource(run: &RunContext, path: &str) -> Result<String, AgentError> {
    if !negotiated(&run.extensions) {
        return Err(invalid("A2UI not negotiated"));
    }
    let catalogs = catalogs(run);
    match path {
        "references/components.md" => Ok(COMPONENTS.into()),
        "references/examples.json" => Ok(EXAMPLES.replace(
            AGENT_CATALOG_ID,
            catalogs[0].1["catalogId"].as_str().unwrap(),
        )),
        "protocol.json" => Ok(include_str!("schemas/server_to_client.json").into()),
        "common-types.json" => Ok(include_str!("schemas/common_types.json").into()),
        "catalog.json" => {
            let index=catalogs.iter().map(|(alias,catalog)| {
                let components=catalog["components"].as_object().unwrap().keys().collect::<Vec<_>>();
                json!({"catalogId":catalog["catalogId"],"components":components,"componentResourceTemplate":format!("components/{alias}/{{component}}.json"),"commonComponentProperties":catalog["$defs"]["CatalogComponentCommon"]})
            }).collect::<Vec<_>>();
            Ok(json!({"version":VERSION,"catalogs":index,"commonTypesResource":"common-types.json","functionExpressions":false}).to_string())
        }
        _ => {
            for (alias, catalog) in &catalogs {
                for (name, schema) in catalog["components"].as_object().unwrap() {
                    if path == format!("components/{alias}/{name}.json") {
                        return Ok(json!({"catalogId":catalog["catalogId"],"component":schema})
                            .to_string());
                    }
                }
            }
            Err(invalid("Unknown A2UI skill resource"))
        }
    }
}
#[async_trait]
impl SkillProvider for Guidance {
    async fn catalog(&self, run: &RunContext) -> Result<SkillCatalog, AgentError> {
        Ok(SkillCatalog {
            skills: if negotiated(&run.extensions) {
                vec![summary()]
            } else {
                vec![]
            },
            diagnostics: vec![],
        })
    }
    async fn load(&self, run: &RunContext, name: &str) -> Result<SkillDetail, AgentError> {
        if name != A2UI_SKILL_NAME || !negotiated(&run.extensions) {
            return Err(invalid("A2UI skill unavailable"));
        }
        let files = resource_paths(run)
            .into_iter()
            .map(|path| {
                Ok(SkillFile {
                    size_bytes: resource(run, &path)?.len() as u64,
                    path,
                })
            })
            .collect::<Result<_, AgentError>>()?;
        Ok(SkillDetail {
            skill: summary(),
            skill_markdown: SKILL.into(),
            files,
        })
    }
    async fn read_resource(
        &self,
        run: &RunContext,
        name: &str,
        path: &str,
    ) -> Result<SkillResource, AgentError> {
        if name != A2UI_SKILL_NAME {
            return Err(invalid("Unknown A2UI skill"));
        }
        Ok(SkillResource {
            skill_name: name.into(),
            path: path.into(),
            content: resource(run, path)?,
        })
    }
}
