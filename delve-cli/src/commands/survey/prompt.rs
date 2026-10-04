//! How `delve survey` asks its questions: a small trait, so the survey can be
//! driven by a real terminal (dialoguer) or, in tests, by a script.

/// Asks questions and shows notes. Every question goes to stderr, so
/// `delve survey --print` can still write the file to stdout.
pub(crate) trait Prompter {
    /// A line of text. `default` is used when the answer is empty; with no
    /// default, an empty answer is allowed only when `allow_empty`.
    fn text(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        allow_empty: bool,
    ) -> anyhow::Result<String>;

    /// Yes or no.
    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool>;

    /// One of `items`, by index.
    fn select(&mut self, prompt: &str, items: &[String], default: usize) -> anyhow::Result<usize>;

    /// Any number of `items`, by index, starting from `checked`.
    fn multi_select(
        &mut self,
        prompt: &str,
        items: &[String],
        checked: &[bool],
    ) -> anyhow::Result<Vec<usize>>;

    /// Something to read, such as why a question matters or a warning.
    fn note(&mut self, text: &str);
}

/// Asks on the terminal with dialoguer.
pub(crate) struct TerminalPrompter {
    term: dialoguer::console::Term,
    theme: dialoguer::theme::ColorfulTheme,
}

impl TerminalPrompter {
    pub(crate) fn new() -> Self {
        Self {
            term: dialoguer::console::Term::stderr(),
            theme: dialoguer::theme::ColorfulTheme::default(),
        }
    }
}

impl Prompter for TerminalPrompter {
    fn text(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        allow_empty: bool,
    ) -> anyhow::Result<String> {
        let mut input = dialoguer::Input::<String>::with_theme(&self.theme)
            .with_prompt(prompt)
            .allow_empty(allow_empty || default.is_some());
        if let Some(default) = default {
            input = input.default(default.to_string());
        }
        Ok(input.interact_text_on(&self.term)?)
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        Ok(dialoguer::Confirm::with_theme(&self.theme)
            .with_prompt(prompt)
            .default(default)
            .interact_on(&self.term)?)
    }

    fn select(&mut self, prompt: &str, items: &[String], default: usize) -> anyhow::Result<usize> {
        Ok(dialoguer::Select::with_theme(&self.theme)
            .with_prompt(prompt)
            .items(items)
            .default(default)
            .interact_on(&self.term)?)
    }

    fn multi_select(
        &mut self,
        prompt: &str,
        items: &[String],
        checked: &[bool],
    ) -> anyhow::Result<Vec<usize>> {
        Ok(dialoguer::MultiSelect::with_theme(&self.theme)
            .with_prompt(format!("{prompt} (space to toggle, enter to accept)"))
            .items(items)
            .defaults(checked)
            .interact_on(&self.term)?)
    }

    fn note(&mut self, text: &str) {
        // A closed stderr only loses the note; the questions would fail anyway.
        let _ = self.term.write_line(text);
    }
}

/// One scripted answer.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) enum Answer {
    /// Take whatever the question offers by default.
    Default,
    Text(&'static str),
    Yes,
    No,
    /// The item whose label starts with this text.
    Pick(&'static str),
    /// Exactly the items whose labels start with these texts.
    Check(&'static [&'static str]),
}

/// Answers from a script, for tests. Records every prompt and note, and
/// fails loudly when the script and the questions disagree.
#[cfg(test)]
pub(crate) struct ScriptedPrompter {
    answers: std::collections::VecDeque<Answer>,
    /// Every prompt asked and note shown, in order.
    pub(crate) transcript: Vec<String>,
}

#[cfg(test)]
impl ScriptedPrompter {
    pub(crate) fn new(answers: impl IntoIterator<Item = Answer>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            transcript: Vec::new(),
        }
    }

    /// Whether the script was used up, so a test can tell a short survey
    /// from one that skipped questions.
    pub(crate) fn finished(&self) -> bool {
        self.answers.is_empty()
    }

    fn next(&mut self, prompt: &str) -> Answer {
        self.transcript.push(format!("? {prompt}"));
        self.answers.pop_front().unwrap_or_else(|| {
            panic!(
                "the script ran out at: {prompt}\ntranscript:\n{}",
                self.transcript.join("\n")
            )
        })
    }

    fn index_of(items: &[String], label: &str, prompt: &str) -> usize {
        items
            .iter()
            .position(|i| i.starts_with(label))
            .unwrap_or_else(|| panic!("no item starts with {label:?} at {prompt}: {items:?}"))
    }
}

#[cfg(test)]
impl Prompter for ScriptedPrompter {
    fn text(
        &mut self,
        prompt: &str,
        default: Option<&str>,
        allow_empty: bool,
    ) -> anyhow::Result<String> {
        match self.next(prompt) {
            Answer::Default => match default {
                Some(d) => Ok(d.to_string()),
                None if allow_empty => Ok(String::new()),
                None => panic!("{prompt} has no default"),
            },
            Answer::Text(t) => Ok(t.to_string()),
            other => panic!("{prompt} wants text, the script has {other:?}"),
        }
    }

    fn confirm(&mut self, prompt: &str, default: bool) -> anyhow::Result<bool> {
        match self.next(prompt) {
            Answer::Default => Ok(default),
            Answer::Yes => Ok(true),
            Answer::No => Ok(false),
            other => panic!("{prompt} wants yes or no, the script has {other:?}"),
        }
    }

    fn select(&mut self, prompt: &str, items: &[String], default: usize) -> anyhow::Result<usize> {
        match self.next(prompt) {
            Answer::Default => Ok(default),
            Answer::Pick(label) => Ok(Self::index_of(items, label, prompt)),
            other => panic!("{prompt} wants a choice, the script has {other:?}"),
        }
    }

    fn multi_select(
        &mut self,
        prompt: &str,
        items: &[String],
        checked: &[bool],
    ) -> anyhow::Result<Vec<usize>> {
        match self.next(prompt) {
            Answer::Default => Ok(checked
                .iter()
                .enumerate()
                .filter(|(_, c)| **c)
                .map(|(i, _)| i)
                .collect()),
            Answer::Check(labels) => Ok(labels
                .iter()
                .map(|l| Self::index_of(items, l, prompt))
                .collect()),
            other => panic!("{prompt} wants choices, the script has {other:?}"),
        }
    }

    fn note(&mut self, text: &str) {
        self.transcript.push(text.to_string());
    }
}
