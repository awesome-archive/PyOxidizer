// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use {
    crate::starlark::file_resource::{FileContentValue, FileManifestValue},
    anyhow::Context,
    starlark::{
        environment::TypeValues,
        values::{
            error::{RuntimeError, ValueError, INCORRECT_PARAMETER_TYPE_ERROR_CODE},
            none::NoneType,
            {Mutable, TypedValue, Value, ValueResult},
        },
        {
            starlark_fun, starlark_module, starlark_parse_param_type, starlark_signature,
            starlark_signature_extraction, starlark_signatures,
        },
    },
    starlark_dialect_build_targets::{
        get_context_value, EnvironmentContext, ResolvedTarget, ResolvedTargetValue, RunMode,
    },
    std::path::PathBuf,
    tugger_apple_bundle::MacOsApplicationBundleBuilder,
    tugger_file_manifest::{FileData, FileManifestError},
};

fn to_runtime_error(err: anyhow::Error, label: impl ToString) -> ValueError {
    ValueError::Runtime(RuntimeError {
        code: "TUGGER_MACOS_APPLICATION_BUNDLE",
        message: format!("{:?}", err),
        label: label.to_string(),
    })
}

fn from_file_manifest_error(err: FileManifestError, label: impl ToString) -> ValueError {
    ValueError::Runtime(RuntimeError {
        code: "TUGGER_MACOS_APPLICATION_BUNDLE",
        message: format!("{:?}", err),
        label: label.to_string(),
    })
}

#[derive(Clone, Debug)]
pub struct MacOsApplicationBundleBuilderValue {
    pub inner: MacOsApplicationBundleBuilder,
}

impl TypedValue for MacOsApplicationBundleBuilderValue {
    type Holder = Mutable<MacOsApplicationBundleBuilderValue>;
    const TYPE: &'static str = "MacOsApplicationBundleBuilder";

    fn values_for_descendant_check_and_freeze(&self) -> Box<dyn Iterator<Item = Value>> {
        Box::new(std::iter::empty())
    }
}

impl MacOsApplicationBundleBuilderValue {
    pub fn new_from_args(bundle_name: String) -> ValueResult {
        let inner = MacOsApplicationBundleBuilder::new(bundle_name)
            .map_err(|e| to_runtime_error(e, "MacOsApplicationBundleBuilder()"))?;

        Ok(Value::new(MacOsApplicationBundleBuilderValue { inner }))
    }

    pub fn add_icon(&mut self, path: String) -> ValueResult {
        self.inner
            .add_icon(FileData::from(PathBuf::from(path)))
            .map_err(|e| to_runtime_error(e, "add_icon()"))?;

        Ok(Value::new(NoneType::None))
    }

    pub fn add_manifest(&mut self, manifest: FileManifestValue) -> ValueResult {
        for (path, entry) in manifest.manifest.iter_entries() {
            self.inner
                .add_file(PathBuf::from("Contents").join(path), entry.clone())
                .with_context(|| format!("adding {}", path.display()))
                .map_err(|e| to_runtime_error(e, "add_manifest()"))?;
        }

        Ok(Value::new(NoneType::None))
    }

    pub fn add_macos_file(&mut self, path: String, content: FileContentValue) -> ValueResult {
        self.inner
            .add_file_macos(path, content.content)
            .map_err(|e| from_file_manifest_error(e, "add_macos_file()"))?;

        Ok(Value::new(NoneType::None))
    }

    pub fn add_macos_manifest(&mut self, manifest: FileManifestValue) -> ValueResult {
        for (path, entry) in manifest.manifest.iter_entries() {
            self.inner
                .add_file_macos(path, entry.clone())
                .with_context(|| format!("adding {}", path.display()))
                .map_err(|e| to_runtime_error(e, "add_macos_manifest()"))?;
        }

        Ok(Value::new(NoneType::None))
    }

    pub fn add_resources_file(&mut self, path: String, content: FileContentValue) -> ValueResult {
        self.inner
            .add_file_resources(path, content.content)
            .map_err(|e| from_file_manifest_error(e, "add_resources_file()"))?;

        Ok(Value::new(NoneType::None))
    }

    pub fn add_resources_manifest(&mut self, manifest: FileManifestValue) -> ValueResult {
        for (path, entry) in manifest.manifest.iter_entries() {
            self.inner
                .add_file_resources(path, entry.clone())
                .with_context(|| format!("adding {}", path.display()))
                .map_err(|e| to_runtime_error(e, "add_file_resources()"))?;
        }

        Ok(Value::new(NoneType::None))
    }

    pub fn set_info_plist_key(&mut self, key: String, value: Value) -> ValueResult {
        let value: plist::Value = match value.get_type() {
            "bool" => value.to_bool().into(),
            "int" => value.to_int()?.into(),
            "string" => value.to_string().into(),
            t => {
                return Err(ValueError::from(RuntimeError {
                    code: INCORRECT_PARAMETER_TYPE_ERROR_CODE,
                    message: format!("function expects a bool, int, or string; got {}", t),
                    label: "set_info_plist_key()".to_string(),
                }))
            }
        };

        self.inner
            .set_info_plist_key(key, value)
            .map_err(|e| to_runtime_error(e, "set_info_plist_key()"))?;

        Ok(Value::new(NoneType::None))
    }

    pub fn set_info_plist_required_keys(
        &mut self,
        display_name: String,
        identifier: String,
        version: String,
        signature: String,
        executable: String,
    ) -> ValueResult {
        self.inner
            .set_info_plist_required_keys(display_name, identifier, version, signature, executable)
            .map_err(|e| to_runtime_error(e, "set_info_plist_required_keys()"))?;

        Ok(Value::new(NoneType::None))
    }

    pub fn build(&self, type_values: &TypeValues, target: String) -> ValueResult {
        let context_value = get_context_value(type_values)?;
        let context = context_value
            .downcast_ref::<EnvironmentContext>()
            .ok_or(ValueError::IncorrectParameterType)?;

        let output_path = context.target_build_path(&target);

        let bundle_path = self
            .inner
            .materialize_bundle(&output_path)
            .map_err(|e| to_runtime_error(e, "build()"))?;

        Ok(Value::new(ResolvedTargetValue {
            inner: ResolvedTarget {
                run_mode: RunMode::Path { path: bundle_path },
                output_path,
            },
        }))
    }
}

starlark_module! { macos_application_bundle_builder_module =>
    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder(bundle_name: String) {
        MacOsApplicationBundleBuilderValue::new_from_args(bundle_name)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.add_icon(this, path: String) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.add_icon(path)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.add_manifest(this, manifest: FileManifestValue) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.add_manifest(manifest)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.add_macos_file(
        this,
        path: String,
        content: FileContentValue)
    {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.add_macos_file(path, content)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.add_macos_manifest(this, manifest: FileManifestValue) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.add_macos_manifest(manifest)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.add_resources_file(
        this,
        path: String,
        content: FileContentValue
    ) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.add_resources_file(path, content)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.add_resources_manifest(this, manifest: FileManifestValue) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.add_resources_manifest(manifest)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.set_info_plist_key(this, key: String, value: Value) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.set_info_plist_key(key, value)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.set_info_plist_required_keys(
        this,
        display_name: String,
        identifier: String,
        version: String,
        signature: String,
        executable: String
    ) {
        let mut this = this.downcast_mut::<MacOsApplicationBundleBuilderValue>().unwrap().unwrap();
        this.set_info_plist_required_keys(display_name, identifier, version, signature, executable)
    }

    #[allow(non_snake_case)]
    MacOsApplicationBundleBuilder.build(env env, this, target: String) {
        let this = this.downcast_ref::<MacOsApplicationBundleBuilderValue>().unwrap();
        this.build(env, target)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, crate::starlark::testutil::*, anyhow::Result};

    #[test]
    fn constructor() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;

        let builder = env.eval("MacOsApplicationBundleBuilder('myapp')")?;
        assert_eq!(builder.get_type(), MacOsApplicationBundleBuilderValue::TYPE);

        Ok(())
    }

    #[test]
    fn set_info_plist_required_keys() -> Result<()> {
        let mut env = StarlarkEnvironment::new()?;

        env.eval("builder = MacOsApplicationBundleBuilder('myapp')")?;
        env.eval("builder.set_info_plist_required_keys('My App', 'com.example.my_app', '0.1', 'myap', 'myapp')")?;

        let builder_value = env.eval("builder")?;
        let builder = builder_value
            .downcast_ref::<MacOsApplicationBundleBuilderValue>()
            .unwrap();

        assert_eq!(
            builder.inner.get_info_plist_key("CFBundleDisplayName")?,
            Some("My App".into())
        );
        assert_eq!(
            builder.inner.get_info_plist_key("CFBundleIdentifier")?,
            Some("com.example.my_app".into())
        );
        assert_eq!(
            builder.inner.get_info_plist_key("CFBundleVersion")?,
            Some("0.1".into())
        );
        assert_eq!(
            builder.inner.get_info_plist_key("CFBundleSignature")?,
            Some("myap".into())
        );
        assert_eq!(
            builder.inner.get_info_plist_key("CFBundleExecutable")?,
            Some("myapp".into())
        );

        Ok(())
    }
}
