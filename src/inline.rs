//! Python written directly inside templates.
//!
//! * `{{ py("sum(o['total'] for o in orders)") }}` — evaluate a snippet and
//!   use the value of its final expression.
//! * `{% call py_block(rows=rows) %}...python...{% endcall %}` — run a block
//!   and emit whatever it `print()`s.
//!
//! Names a snippet does not define and was not passed as keyword arguments
//! are looked up in the template context, so most snippets need no
//! arguments at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use minijinja::value::{Rest, ValueOrKwargs};
use minijinja::{Environment, Error, ErrorKind, State, Value};
use monty::MontyRun;
use monty_types::{CompileOptions, MontyObject};

use crate::convert::{from_monty, to_monty};
use crate::error::PythonError;
use crate::runtime::{self, Config, HOST_FUNCTIONS, NameResolution, Outcome, Script, SharedProgram};

const SCRIPT: &str = "<template>";

/// Compiled snippets keyed by source and keyword-argument names (input names
/// are fixed at compile time). Template sources are finite, so the cache is
/// bounded by the templates in use; it never needs eviction in practice.
type Cache = Mutex<HashMap<(String, Vec<String>), Arc<SharedProgram>>>;

pub struct Inline {
    config: Config,
    cache: Cache,
}

impl Inline {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn install(self: Arc<Self>, env: &mut Environment<'_>) {
        let this = Arc::clone(&self);
        env.add_function("py", move |state: &mut State, args: Rest<ValueOrKwargs>| {
            this.py(state, &args.into_values())
        });
        env.add_function("py_block", move |state: &mut State, args: Rest<ValueOrKwargs>| {
            self.py_block(state, &args.into_values())
        });
    }

    fn py(&self, state: &mut State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        let Some((source, rest)) = args.split_first().and_then(|(s, rest)| Some((s.as_str()?, rest))) else {
            return Err(Error::new(
                ErrorKind::MissingArgument,
                "py() expects python source as its first argument",
            ));
        };
        let outcome = self.run(state, source, named_inputs(rest, &[])?)?;
        from_monty(outcome.value.as_ref())
    }

    fn py_block(&self, state: &mut State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        let caller = kwarg(args, "caller").ok_or_else(|| {
            Error::new(
                ErrorKind::MissingArgument,
                "py_block() must be used as {% call py_block() %}...{% endcall %}",
            )
        })?;
        let safe = kwarg(args, "safe").is_some_and(|v| v.is_true());
        // `caller` and `safe` configure the block; they are not Python inputs.
        let inputs = named_inputs(args, &["caller", "safe"])?;
        let source = dedent(&caller.call(state, &[])?.to_string());
        let printed = self.run(state, &source, inputs)?.printed;
        Ok(if safe {
            Value::from_safe_string(printed)
        } else {
            Value::from(printed)
        })
    }

    fn run(
        &self,
        state: &mut State<'_, '_>,
        source: &str,
        inputs: Vec<(String, MontyObject)>,
    ) -> Result<Outcome, Error> {
        let (names, mut values): (Vec<String>, Vec<MontyObject>) = inputs.into_iter().unzip();
        let program = self.compile(source, &names)?;
        values.extend(
            HOST_FUNCTIONS
                .iter()
                .map(|name| MontyObject::function(*name, None)),
        );
        let script = Script {
            name: SCRIPT,
            line_offset: 0,
            last_user_line: None,
        };
        runtime::execute(
            &program,
            values,
            &self.config,
            Some(state),
            NameResolution::TemplateContext,
            script,
        )
        .map_err(|err| err.into_template_error("python snippet"))
    }

    fn compile(&self, source: &str, names: &[String]) -> Result<Arc<SharedProgram>, Error> {
        let key = (source.to_owned(), names.to_vec());
        if let Some(program) = self.cache.lock().expect("cache poisoned").get(&key) {
            return Ok(Arc::clone(program));
        }
        let inputs = names
            .iter()
            .cloned()
            .chain(HOST_FUNCTIONS.iter().map(|s| (*s).to_owned()))
            .collect();
        let program = MontyRun::new(source.to_owned(), SCRIPT, inputs, CompileOptions::default())
            .map_err(|exc| PythonError::new(exc, SCRIPT, 0).into_template_error("python snippet"))?
            .with_os_policy(self.config.os_policy.clone());
        let program = Arc::new(SharedProgram::new(program));
        self.cache
            .lock()
            .expect("cache poisoned")
            .insert(key, Arc::clone(&program));
        Ok(program)
    }
}

fn kwarg(args: &[Value], name: &str) -> Option<Value> {
    args.last()
        .filter(|v| v.is_kwargs())
        .and_then(|kwargs| kwargs.get_attr(name).ok())
        .filter(|v| !v.is_undefined())
}

/// Collects keyword arguments as Python inputs, skipping `skip`. Snippets
/// take no positional arguments: inputs need names to be referenced.
fn named_inputs(args: &[Value], skip: &[&str]) -> Result<Vec<(String, MontyObject)>, Error> {
    let (kwargs, positional) = match args.split_last() {
        Some((last, rest)) if last.is_kwargs() => (Some(last), rest),
        _ => (None, args),
    };
    if !positional.is_empty() {
        return Err(Error::new(
            ErrorKind::TooManyArguments,
            "python snippets take keyword arguments only",
        ));
    }
    let mut inputs = Vec::new();
    if let Some(kwargs) = kwargs {
        for key in kwargs.try_iter()? {
            let Some(name) = key.as_str().filter(|name| !skip.contains(name)) else {
                continue;
            };
            inputs.push((name.to_owned(), to_monty(&kwargs.get_item(&key)?)?));
        }
    }
    Ok(inputs)
}

/// Strips the indentation common to all non-blank lines, so a block can be
/// indented to match the surrounding template. Leading and trailing blank
/// lines are dropped.
fn dedent(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let body: Vec<&str> = lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                ""
            } else {
                &line[indent..]
            }
        })
        .collect();
    let start = body.iter().position(|l| !l.is_empty()).unwrap_or(body.len());
    let end = body.iter().rposition(|l| !l.is_empty()).map_or(start, |i| i + 1);
    body[start..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::dedent;

    #[test]
    fn dedent_strips_common_indent_and_blank_edges() {
        let text = "\n    for x in y:\n        print(x)\n\n    done()\n  \n";
        assert_eq!(dedent(text), "for x in y:\n    print(x)\n\ndone()");
    }
}
