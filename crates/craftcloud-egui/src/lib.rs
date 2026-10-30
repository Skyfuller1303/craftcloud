//! Reusable, host-styled CraftCloud dialogs for egui applications.

use egui::{Context, Ui, Window};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    English,
    German,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Text {
    Server,
    Email,
    Password,
    SecondFactor,
    SecondFactorHint,
    SignIn,
    Account,
    Organization,
    SignOut,
    Browse,
    SharedWithMe,
    Workspace,
    Project,
    Files,
    More,
    Save,
    FileName,
    NewFile,
    NewVersion,
    KeepBoth,
    Overwrite,
    ConfirmOverwrite,
    Cancel,
    ReadOnly,
    LockedBy,
    Progress,
    Retry,
    Offline,
    Connect,
    Close,
}

pub trait Translator {
    fn text(&self, key: Text) -> &str;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultTranslator(pub Language);

impl Translator for DefaultTranslator {
    fn text(&self, key: Text) -> &str {
        match (self.0, key) {
            (Language::German, Text::Server) => "Server",
            (Language::German, Text::Email) => "E-Mail",
            (Language::German, Text::Password) => "Passwort",
            (Language::German, Text::SecondFactor) => "Zweiter Faktor",
            (Language::German, Text::SecondFactorHint) => {
                "Sechsstelliger Code der Authenticator-App oder zehnstelliger Wiederherstellungscode"
            }
            (Language::German, Text::SignIn) => "Anmelden",
            (Language::German, Text::Account) => "Konto",
            (Language::German, Text::Organization) => "Organisation",
            (Language::German, Text::SignOut) => "Abmelden",
            (Language::German, Text::Browse) => "Durchsuchen",
            (Language::German, Text::SharedWithMe) => "Für mich freigegeben",
            (Language::German, Text::Workspace) => "Arbeitsbereich",
            (Language::German, Text::Project) => "Projekt",
            (Language::German, Text::Files) => "Dateien",
            (Language::German, Text::More) => "Weitere laden",
            (Language::German, Text::Save) => "Speichern",
            (Language::German, Text::FileName) => "Dateiname",
            (Language::German, Text::NewFile) => "Neue Datei",
            (Language::German, Text::NewVersion) => "Neue Version",
            (Language::German, Text::KeepBoth) => "Beide behalten",
            (Language::German, Text::Overwrite) => "Überschreiben",
            (Language::German, Text::ConfirmOverwrite) => "Überschreiben bestätigen",
            (Language::German, Text::Cancel) => "Abbrechen",
            (Language::German, Text::ReadOnly) => "Schreibgeschützt öffnen",
            (Language::German, Text::LockedBy) => "In Bearbeitung von",
            (Language::German, Text::Progress) => "Fortschritt",
            (Language::German, Text::Retry) => "Erneut hochladen",
            (Language::German, Text::Offline) => {
                "Offline. Lokal gespeichert; Cloud-Upload ausstehend."
            }
            (Language::German, Text::Connect) => "Verbinden",
            (Language::German, Text::Close) => "Schließen",
            (Language::English, Text::Server) => "Server",
            (Language::English, Text::Email) => "Email",
            (Language::English, Text::Password) => "Password",
            (Language::English, Text::SecondFactor) => "Second factor",
            (Language::English, Text::SecondFactorHint) => {
                "Six-digit authenticator code or ten-character recovery code"
            }
            (Language::English, Text::SignIn) => "Sign in",
            (Language::English, Text::Account) => "Account",
            (Language::English, Text::Organization) => "Organization",
            (Language::English, Text::SignOut) => "Sign out",
            (Language::English, Text::Browse) => "Browse",
            (Language::English, Text::SharedWithMe) => "Shared with me",
            (Language::English, Text::Workspace) => "Workspace",
            (Language::English, Text::Project) => "Project",
            (Language::English, Text::Files) => "Files",
            (Language::English, Text::More) => "Load more",
            (Language::English, Text::Save) => "Save",
            (Language::English, Text::FileName) => "File name",
            (Language::English, Text::NewFile) => "New file",
            (Language::English, Text::NewVersion) => "New version",
            (Language::English, Text::KeepBoth) => "Keep both",
            (Language::English, Text::Overwrite) => "Overwrite",
            (Language::English, Text::ConfirmOverwrite) => "Confirm overwrite",
            (Language::English, Text::Cancel) => "Cancel",
            (Language::English, Text::ReadOnly) => "Open read-only",
            (Language::English, Text::LockedBy) => "Being edited by",
            (Language::English, Text::Progress) => "Progress",
            (Language::English, Text::Retry) => "Retry upload",
            (Language::English, Text::Offline) => "Offline. Saved locally; cloud upload pending.",
            (Language::English, Text::Connect) => "Connect",
            (Language::English, Text::Close) => "Close",
        }
    }
}

/// A second-factor code as typed into the sign-in dialog, normalized for the server.
#[derive(Clone, PartialEq, Eq)]
pub enum SecondFactorInput {
    /// Six digits from an authenticator app.
    Totp(String),
    /// Ten-character one-use recovery code, upper case.
    Recovery(String),
}
impl std::fmt::Debug for SecondFactorInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Totp(_) => f.write_str("SecondFactorInput::Totp(***)"),
            Self::Recovery(_) => f.write_str("SecondFactorInput::Recovery(***)"),
        }
    }
}

/// Classifies typed input: six digits are an authenticator code, ten letters or digits
/// a recovery code. Spaces and dashes (as people group codes) are ignored. Anything else
/// is rejected before it is sent, which spares an attempt on the server's limited budget.
pub fn classify_second_factor(input: &str) -> Option<SecondFactorInput> {
    let compact: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if compact.len() == 6 && compact.bytes().all(|b| b.is_ascii_digit()) {
        Some(SecondFactorInput::Totp(compact))
    } else if compact.len() == 10 && compact.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Some(SecondFactorInput::Recovery(compact.to_ascii_uppercase()))
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialog {
    Server,
    Login,
    Account,
    Browse,
    Save,
    Conflict,
    Lock,
    Progress,
    Offline,
}

#[derive(Clone, PartialEq)]
pub enum Action {
    AddServer(String),
    Login {
        server: String,
        email: String,
        password: String,
        second_factor: Option<String>,
    },
    Logout,
    SelectOrganization(String),
    SelectWorkspace(String),
    SelectProject(String),
    OpenFile(String),
    OpenShared,
    More,
    SaveNew {
        name: String,
    },
    SaveVersion {
        file_id: String,
    },
    KeepBoth,
    Overwrite,
    Cancel,
    OpenReadOnly,
    RetryUpload,
}

#[derive(Clone, Default)]
pub struct PickerState {
    pub dialog: Option<Dialog>,
    pub server_url: String,
    pub email: String,
    pub password: String,
    pub second_factor: String,
    pub requires_second_factor: bool,
    pub second_factor_challenge_id: Option<String>,
    pub account_name: String,
    pub organization_name: String,
    pub organizations: Vec<(String, String)>,
    pub workspaces: Vec<(String, String)>,
    pub projects: Vec<(String, String)>,
    pub files: Vec<(String, String)>,
    pub more_available: bool,
    pub locked_by: Option<String>,
    pub progress: f32,
    pub file_name: String,
    pub selected_file: Option<String>,
    pub error: Option<String>,
    pub pending_overwrite_confirmation: bool,
}

impl PickerState {
    pub fn show(&mut self, ctx: &Context, tr: &impl Translator) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(dialog) = self.dialog else {
            return actions;
        };
        let title = match dialog {
            Dialog::Server => tr.text(Text::Server),
            Dialog::Login => tr.text(Text::SignIn),
            Dialog::Account => tr.text(Text::Account),
            Dialog::Browse => tr.text(Text::Browse),
            Dialog::Save => tr.text(Text::Save),
            Dialog::Conflict => tr.text(Text::Save),
            Dialog::Lock => tr.text(Text::LockedBy),
            Dialog::Progress => tr.text(Text::Progress),
            Dialog::Offline => tr.text(Text::Offline),
        };
        Window::new(title)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                self.show_dialog(ui, tr, dialog, &mut actions);
            });
        actions
    }

    fn show_dialog(
        &mut self,
        ui: &mut Ui,
        tr: &impl Translator,
        dialog: Dialog,
        out: &mut Vec<Action>,
    ) {
        if let Some(error) = &self.error {
            ui.label(error);
        }
        match dialog {
            Dialog::Server => {
                ui.label(tr.text(Text::Server));
                ui.text_edit_singleline(&mut self.server_url);
                if ui.button(tr.text(Text::Connect)).clicked() {
                    out.push(Action::AddServer(self.server_url.clone()));
                }
            }
            Dialog::Login => {
                ui.label(tr.text(Text::Server));
                ui.monospace(&self.server_url);
                ui.label(tr.text(Text::Email));
                if self.requires_second_factor {
                    // The password step is done; only the code is asked for now.
                    ui.monospace(&self.email);
                    ui.label(tr.text(Text::SecondFactor));
                    ui.add(egui::TextEdit::singleline(&mut self.second_factor).password(true));
                    ui.small(tr.text(Text::SecondFactorHint));
                    if ui.button(tr.text(Text::SignIn)).clicked() {
                        match classify_second_factor(&self.second_factor) {
                            Some(SecondFactorInput::Totp(code))
                            | Some(SecondFactorInput::Recovery(code)) => {
                                self.error = None;
                                out.push(Action::Login {
                                    server: self.server_url.clone(),
                                    email: self.email.clone(),
                                    password: String::new(),
                                    second_factor: Some(code),
                                });
                            }
                            None => {
                                self.error = Some(tr.text(Text::SecondFactorHint).to_owned());
                            }
                        }
                        self.second_factor.clear();
                    }
                    if ui.button(tr.text(Text::Cancel)).clicked() {
                        // Back to the password step; the open challenge is abandoned.
                        self.requires_second_factor = false;
                        self.second_factor_challenge_id = None;
                        self.second_factor.clear();
                        self.error = None;
                    }
                } else {
                    ui.text_edit_singleline(&mut self.email);
                    ui.label(tr.text(Text::Password));
                    ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                    if ui.button(tr.text(Text::SignIn)).clicked() {
                        out.push(Action::Login {
                            server: self.server_url.clone(),
                            email: self.email.clone(),
                            password: self.password.clone(),
                            second_factor: None,
                        });
                        self.password.clear();
                    }
                }
            }
            Dialog::Account => {
                ui.heading(&self.account_name);
                ui.label(format!(
                    "{}: {}",
                    tr.text(Text::Organization),
                    self.organization_name
                ));
                if ui.button(tr.text(Text::SignOut)).clicked() {
                    out.push(Action::Logout);
                }
            }
            Dialog::Browse => {
                if ui.button(tr.text(Text::SharedWithMe)).clicked() {
                    out.push(Action::OpenShared);
                }
                for (id, name) in &self.organizations {
                    if ui
                        .button(format!("{}: {name}", tr.text(Text::Organization)))
                        .clicked()
                    {
                        out.push(Action::SelectOrganization(id.clone()));
                    }
                }
                for (id, name) in &self.workspaces {
                    if ui
                        .button(format!("{}: {name}", tr.text(Text::Workspace)))
                        .clicked()
                    {
                        out.push(Action::SelectWorkspace(id.clone()));
                    }
                }
                for (id, name) in &self.projects {
                    if ui
                        .button(format!("{}: {name}", tr.text(Text::Project)))
                        .clicked()
                    {
                        out.push(Action::SelectProject(id.clone()));
                    }
                }
                ui.separator();
                ui.label(tr.text(Text::Files));
                for (id, name) in &self.files {
                    if ui.button(name).clicked() {
                        out.push(Action::OpenFile(id.clone()));
                    }
                }
                if self.more_available && ui.button(tr.text(Text::More)).clicked() {
                    out.push(Action::More);
                }
                if ui.button(tr.text(Text::Cancel)).clicked() {
                    out.push(Action::Cancel);
                    self.dialog = None;
                }
            }
            Dialog::Save => {
                ui.label(tr.text(Text::FileName));
                ui.text_edit_singleline(&mut self.file_name);
                if ui.button(tr.text(Text::NewFile)).clicked() {
                    out.push(Action::SaveNew {
                        name: self.file_name.clone(),
                    });
                }
                if let Some(id) = &self.selected_file
                    && ui.button(tr.text(Text::NewVersion)).clicked()
                {
                    out.push(Action::SaveVersion {
                        file_id: id.clone(),
                    });
                }
                if ui.button(tr.text(Text::Cancel)).clicked() {
                    out.push(Action::Cancel);
                    self.dialog = None;
                }
            }
            Dialog::Conflict => {
                if ui.button(tr.text(Text::KeepBoth)).clicked() {
                    out.push(Action::KeepBoth);
                }
                if self.pending_overwrite_confirmation {
                    ui.label(tr.text(Text::ConfirmOverwrite));
                    if ui.button(tr.text(Text::ConfirmOverwrite)).clicked() {
                        out.push(Action::Overwrite);
                    }
                } else if ui.button(tr.text(Text::Overwrite)).clicked() {
                    self.pending_overwrite_confirmation = true;
                }
                if ui.button(tr.text(Text::Cancel)).clicked() {
                    out.push(Action::Cancel);
                    self.dialog = None;
                }
            }
            Dialog::Lock => {
                if let Some(owner) = &self.locked_by {
                    ui.label(format!("{} {owner}", tr.text(Text::LockedBy)));
                }
                if ui.button(tr.text(Text::ReadOnly)).clicked() {
                    out.push(Action::OpenReadOnly);
                }
                if ui.button(tr.text(Text::Cancel)).clicked() {
                    out.push(Action::Cancel);
                    self.dialog = None;
                }
            }
            Dialog::Progress => {
                ui.add(egui::ProgressBar::new(self.progress.clamp(0.0, 1.0)).show_percentage());
                if ui.button(tr.text(Text::Cancel)).clicked() {
                    out.push(Action::Cancel);
                }
            }
            Dialog::Offline => {
                ui.label(tr.text(Text::Offline));
                if ui.button(tr.text(Text::Retry)).clicked() {
                    out.push(Action::RetryUpload);
                }
                if ui.button(tr.text(Text::Close)).clicked() {
                    self.dialog = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_context_renders_conflict_and_confirmation_state() {
        let ctx = Context::default();
        let mut state = PickerState {
            dialog: Some(Dialog::Conflict),
            ..Default::default()
        };
        let output = ctx.run_ui(Default::default(), |ui| {
            state.show(&ui.ctx().clone(), &DefaultTranslator::default());
        });
        output.drop_without_applying_deltas();
        assert!(!state.pending_overwrite_confirmation);
        state.pending_overwrite_confirmation = true;
        let output = ctx.run_ui(Default::default(), |ui| {
            state.show(&ui.ctx().clone(), &DefaultTranslator::default());
        });
        output.drop_without_applying_deltas();
        assert!(state.pending_overwrite_confirmation);
    }

    #[test]
    fn translations_cover_required_language_defaults() {
        assert_eq!(
            DefaultTranslator(Language::German).text(Text::KeepBoth),
            "Beide behalten"
        );
        assert_eq!(
            DefaultTranslator(Language::English).text(Text::KeepBoth),
            "Keep both"
        );
    }

    #[test]
    fn all_dialogs_render_without_a_gpu() {
        let ctx = Context::default();
        let mut state = PickerState::default();
        for dialog in [
            Dialog::Server,
            Dialog::Login,
            Dialog::Account,
            Dialog::Browse,
            Dialog::Save,
            Dialog::Conflict,
            Dialog::Lock,
            Dialog::Progress,
            Dialog::Offline,
        ] {
            state.dialog = Some(dialog);
            let output = ctx.run_ui(Default::default(), |ui| {
                state.show(&ui.ctx().clone(), &DefaultTranslator::default());
            });
            output.drop_without_applying_deltas();
        }
    }

    #[test]
    fn second_factor_input_is_classified_and_normalized() {
        assert_eq!(
            classify_second_factor(" 123 456 "),
            Some(SecondFactorInput::Totp("123456".into()))
        );
        assert_eq!(
            classify_second_factor("abcde-fg2h3"),
            Some(SecondFactorInput::Recovery("ABCDEFG2H3".into()))
        );
        for rejected in [
            "",
            "12345",
            "1234567",
            "abcdef",
            "abcde-fg2h",
            "abcde fg2h3x",
            "12 34 5é",
        ] {
            assert_eq!(classify_second_factor(rejected), None, "{rejected:?}");
        }
        assert_eq!(
            format!("{:?}", SecondFactorInput::Totp("123456".into())),
            "SecondFactorInput::Totp(***)"
        );
    }

    #[test]
    fn second_factor_stage_renders_without_a_gpu() {
        let ctx = Context::default();
        let mut state = PickerState {
            dialog: Some(Dialog::Login),
            requires_second_factor: true,
            second_factor_challenge_id: Some("challenge".into()),
            ..Default::default()
        };
        for language in [Language::English, Language::German] {
            let output = ctx.run_ui(Default::default(), |ui| {
                state.show(&ui.ctx().clone(), &DefaultTranslator(language));
            });
            output.drop_without_applying_deltas();
        }
        assert!(state.requires_second_factor);
    }
}
