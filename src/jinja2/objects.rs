//! Jinja2's helper globals that differ from MiniJinja's: the stateful
//! `cycler` and `joiner`, which keep state across calls within a render
//! (Python code cannot, under per-call isolation), and a `range` that prints
//! as Python's does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use minijinja::value::{Enumerator, Object, ObjectRepr, Rest, from_args};
use minijinja::{Environment, Error, ErrorKind, State, Value};

/// MiniJinja's own cap, kept so a template cannot make a render iterate
/// without bound.
const MAX_RANGE_LEN: i64 = 100_000;

pub(super) fn install(env: &mut Environment<'_>) {
    env.add_function("range", PyRange::create);
    env.add_function("cycler", |items: Rest<Value>| -> Result<Value, Error> {
        if items.is_empty() {
            return Err(Error::new(
                ErrorKind::MissingArgument,
                "cycler requires at least one item",
            ));
        }
        Ok(Value::from_object(Cycler {
            items: items.0,
            position: AtomicUsize::new(0),
        }))
    });
    env.add_function("joiner", |sep: Option<Value>| {
        let sep = sep.unwrap_or_else(|| Value::from(", "));
        let used = AtomicBool::new(false);
        // First call yields "", later calls the separator.
        Value::from_function(move || -> Value {
            if used.swap(true, Ordering::Relaxed) {
                sep.clone()
            } else {
                Value::from("")
            }
        })
    });
}

/// `cycler(*items)`: `.next()` returns the current item and advances,
/// `.current` reads it, `.reset()` starts over.
#[derive(Debug)]
struct Cycler {
    items: Vec<Value>,
    position: AtomicUsize,
}

impl Cycler {
    fn current(&self) -> Value {
        self.items[self.position.load(Ordering::Relaxed) % self.items.len()].clone()
    }
}

impl Object for Cycler {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        (key.as_str()? == "current").then(|| self.current())
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        let () = from_args(args)?;
        match method {
            "next" => {
                let current = self.current();
                self.position.fetch_add(1, Ordering::Relaxed);
                Ok(current)
            }
            "reset" => {
                self.position.store(0, Ordering::Relaxed);
                Ok(Value::from(()))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

/// Python's `range`: lazy, indexable, and printed as `range(0, 3)` rather
/// than as the list it produces, matching Jinja2.
#[derive(Debug)]
pub(super) struct PyRange {
    start: i64,
    stop: i64,
    step: i64,
}

impl PyRange {
    /// The `range()` global: builds the range as a template value.
    fn create(first: i64, stop: Option<i64>, step: Option<i64>) -> Result<Value, Error> {
        let (start, stop) = match stop {
            Some(stop) => (first, stop),
            None => (0, first),
        };
        let step = step.unwrap_or(1);
        if step == 0 {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "range() arg 3 must not be zero",
            ));
        }
        let range = Self { start, stop, step };
        if range.len() > MAX_RANGE_LEN {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "range has too many elements",
            ));
        }
        Ok(Value::from_object(range))
    }

    /// Python's length formula, in i128 so extreme bounds cannot overflow.
    fn len(&self) -> i64 {
        let (start, stop, step) = (
            i128::from(self.start),
            i128::from(self.stop),
            i128::from(self.step),
        );
        let len = if step > 0 && start < stop {
            (stop - start - 1) / step + 1
        } else if step < 0 && start > stop {
            (start - stop - 1) / -step + 1
        } else {
            0
        };
        i64::try_from(len).unwrap_or(i64::MAX)
    }
}

impl Object for PyRange {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Seq
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let index = i64::try_from(key.as_usize()?).ok()?;
        (index < self.len()).then(|| Value::from(self.start + index * self.step))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Seq(usize::try_from(self.len()).unwrap_or(0))
    }

    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.step {
            1 => write!(f, "range({}, {})", self.start, self.stop),
            step => write!(f, "range({}, {}, {step})", self.start, self.stop),
        }
    }
}
