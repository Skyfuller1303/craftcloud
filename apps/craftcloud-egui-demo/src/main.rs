use std::{
    collections::HashMap,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use craftcloud_client::dto::{File as CloudFile, Organization, Project, Workspace};
use craftcloud_client::{CloudClient, CloudError, Proof, UploadContent};
use craftcloud_client_native::{
    CloudLink, FileLeaseHeartbeat, KeychainTokenStore, LinksStore, NewFile, SaveOutcome,
    UreqTransport, VersionCache, cache_extension, overwrite_local_first_with_cancel,
    save_local_first_with_cancel, save_new_local_first_with_cancel, wait_for_release,
};
use craftcloud_egui::{
    Action, DefaultTranslator, Dialog, Language, PickerState, SecondFactorInput,
    classify_second_factor,
};
use eframe::egui;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;

enum Request {
    Discover(String),
    Login {
        email: String,
        password: String,
    },
    SecondFactor {
        email: String,
        challenge_id: String,
        proof: Proof,
    },
    List {
        context: ListContext,
        cursor: Option<String>,
    },
    Open {
        file_id: String,
        read_only: bool,
    },
    SaveNew {
        project_id: String,
        name: String,
        path: PathBuf,
    },
    SaveVersion {
        file_id: String,
        path: PathBuf,
    },
    Overwrite {
        file_id: String,
        path: PathBuf,
    },
    Logout,
    CloseFile(String),
    Shutdown,
}

enum Event {
    Discovered,
    LoggedIn {
        display_name: String,
        email: String,
    },
    SecondFactor(String),
    SecondFactorRejected(String),
    Listing {
        context: ListContext,
        items: ListItems,
        next_cursor: Option<String>,
    },
    LeaseLost {
        file_id: String,
        message: String,
    },
    Downloaded {
        file_id: String,
        path: PathBuf,
        read_only: bool,
    },
    Saved(String),
    Retryable {
        file_id: String,
        message: String,
    },
    SaveNewFailed {
        name: String,
        message: String,
    },
    Closed,
    Conflict(String),
    Locked(String),
    LoggedOut,
    /// The local session ended but the server could not be told.
    LoggedOutLocally(String),
    Failed(String),
    Cancelled,
}

/// Results that describe something that already happened on the server. A cancel request
/// that arrives after the commit or the file creation must not replace them with
/// "cancelled": the UI would otherwise forget the new file or version and, for example,
/// create a second remote file on the next save.
fn survives_cancel(event: &Event) -> bool {
    matches!(
        event,
        Event::Saved(_) | Event::Retryable { .. } | Event::Conflict(_)
    )
}

struct Job {
    id: u64,
    request: Request,
    cancelled: Arc<AtomicBool>,
}
struct WorkerEvent {
    id: Option<u64>,
    event: Event,
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    next_cursor: Option<String>,
}
#[derive(Deserialize)]
struct SharedItem {
    project: Option<SharedTarget>,
    file: Option<SharedTarget>,
}
#[derive(Deserialize)]
struct SharedTarget {
    id: String,
    name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ListContext {
    Organizations,
    Shared(String),
    Workspaces(String),
    Projects(String),
    Files(String),
}
enum ListItems {
    Organizations(Vec<(String, String)>),
    Shared {
        projects: Vec<(String, String)>,
        files: Vec<(String, String)>,
    },
    Workspaces(Vec<(String, String)>),
    Projects(Vec<(String, String)>),
    Files(Vec<(String, String)>),
}

const PAGE_BATCH_SIZE: usize = 10;

struct DemoApp {
    picker: PickerState,
    language: Language,
    request: Sender<Job>,
    events: Receiver<WorkerEvent>,
    busy: bool,
    status: String,
    project_id: Option<String>,
    organization_id: Option<String>,
    workspace_id: Option<String>,
    active_list: Option<ListContext>,
    next_cursor: Option<String>,
    keep_both_pending: bool,
    file_name: String,
    local_path: Option<PathBuf>,
    selected_read_only: bool,
    demo_credentials: Option<(String, String)>,
    current_job: Option<(u64, Arc<AtomicBool>)>,
    next_job_id: u64,
    worker: Option<thread::JoinHandle<()>>,
}

impl DemoApp {
    fn new(server: String) -> Self {
        let (request, requests) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("craftcloud-demo-worker".into())
            .spawn(move || worker(requests, events))
            .ok();
        let picker = PickerState {
            server_url: server,
            dialog: Some(Dialog::Server),
            ..Default::default()
        };
        let busy = !picker.server_url.is_empty();
        let mut app = Self {
            picker,
            language: Language::English,
            request,
            events: receiver,
            busy: false,
            status: String::new(),
            project_id: None,
            organization_id: None,
            workspace_id: None,
            active_list: None,
            next_cursor: None,
            keep_both_pending: false,
            file_name: String::new(),
            local_path: None,
            selected_read_only: false,
            demo_credentials: std::env::var("CRAFTCLOUD_DEMO_EMAIL")
                .ok()
                .zip(std::env::var("CRAFTCLOUD_DEMO_PASSWORD").ok()),
            current_job: None,
            next_job_id: 0,
            worker,
        };
        if busy {
            app.send(Request::Discover(app.picker.server_url.clone()));
        }
        app
    }

    fn send(&mut self, request: Request) {
        if self.busy {
            self.status = "Wait for the current cloud action to finish".into();
            return;
        }
        let id = self.next_job_id;
        self.next_job_id = self.next_job_id.wrapping_add(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        if self
            .request
            .send(Job {
                id,
                request,
                cancelled: Arc::clone(&cancelled),
            })
            .is_ok()
        {
            self.current_job = Some((id, cancelled));
            self.busy = true;
            self.picker.dialog = Some(Dialog::Progress);
            self.picker.progress = 0.25;
        } else {
            self.status = "Background worker stopped".into();
        }
    }

    fn clear_browse_descendants(&mut self) {
        self.workspace_id = None;
        self.project_id = None;
        self.picker.workspaces.clear();
        self.picker.projects.clear();
        self.picker.files.clear();
        self.picker.selected_file = None;
        self.picker.more_available = false;
        self.selected_read_only = false;
        self.active_list = None;
        self.next_cursor = None;
    }

    fn clear_server_context(&mut self) {
        self.organization_id = None;
        self.picker.organization_name.clear();
        self.picker.organizations.clear();
        self.keep_both_pending = false;
        self.picker.file_name.clear();
        self.clear_browse_descendants();
    }

    fn prepare_list_context(&mut self, context: &ListContext) {
        match context {
            ListContext::Organizations => self.clear_server_context(),
            ListContext::Shared(org_id) | ListContext::Workspaces(org_id) => {
                self.clear_browse_descendants();
                self.organization_id = Some(org_id.clone());
            }
            ListContext::Projects(workspace_id) => {
                self.project_id = None;
                self.picker.projects.clear();
                self.picker.files.clear();
                self.picker.selected_file = None;
                self.workspace_id = Some(workspace_id.clone());
                self.picker.more_available = false;
                self.next_cursor = None;
            }
            ListContext::Files(project_id) => {
                self.picker.files.clear();
                self.picker.selected_file = None;
                self.project_id = Some(project_id.clone());
                self.picker.more_available = false;
                self.next_cursor = None;
            }
        }
    }

    fn start_list(&mut self, context: ListContext) {
        self.active_list = Some(context.clone());
        self.next_cursor = None;
        self.picker.more_available = false;
        self.send(Request::List {
            context,
            cursor: None,
        });
    }

    fn load_more(&mut self) {
        if let (Some(context), Some(cursor)) = (self.active_list.clone(), self.next_cursor.clone())
        {
            self.send(Request::List {
                context,
                cursor: Some(cursor),
            });
        }
    }

    fn process_events(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.events.try_recv() {
            if message.id.is_none() {
                if let Event::LeaseLost { file_id, message } = message.event {
                    if self.picker.selected_file.as_deref() == Some(&file_id) {
                        self.selected_read_only = true;
                        self.picker.locked_by = Some(message.clone());
                        self.status = format!("Lease lost; cloud file is read-only: {message}");
                    }
                    ctx.request_repaint();
                }
                continue;
            }
            let Some(message_id) = message.id else {
                continue;
            };
            let Some((active_id, cancelled)) = self.current_job.as_ref() else {
                continue;
            };
            if *active_id != message_id {
                continue;
            }
            let close_file_after_cancel = if cancelled.load(Ordering::Acquire) {
                match &message.event {
                    Event::Downloaded { file_id, .. } => Some(file_id.clone()),
                    _ => None,
                }
            } else {
                None
            };
            let cancel_requested = cancelled.load(Ordering::Acquire);
            let event = if cancel_requested && !survives_cancel(&message.event) {
                Event::Cancelled
            } else {
                message.event
            };
            self.current_job = None;
            self.busy = false;
            self.picker.error = None;
            match event {
                Event::Discovered => {
                    self.status = "Server discovery succeeded".into();
                    if let Some((email, password)) = self.demo_credentials.take() {
                        self.picker.email = email.clone();
                        self.send(Request::Login { email, password });
                    } else {
                        self.picker.dialog = Some(Dialog::Login);
                    }
                }
                Event::LoggedIn {
                    display_name,
                    email,
                } => {
                    self.picker.dialog = Some(Dialog::Account);
                    self.picker.email = email;
                    self.picker.account_name = display_name;
                    self.picker.requires_second_factor = false;
                    self.picker.second_factor_challenge_id = None;
                    self.status = "Signed in".into();
                    self.start_list(ListContext::Organizations);
                }
                Event::SecondFactor(challenge) => {
                    self.picker.requires_second_factor = true;
                    self.picker.second_factor_challenge_id = Some(challenge);
                    self.picker.dialog = Some(Dialog::Login);
                    self.status = "Enter second factor".into();
                }
                Event::SecondFactorRejected(message) => {
                    self.picker.dialog = Some(Dialog::Login);
                    self.picker.error = Some(message.clone());
                    self.status = message;
                }
                Event::Listing {
                    context,
                    items,
                    next_cursor,
                } => {
                    let append =
                        self.active_list.as_ref() == Some(&context) && self.next_cursor.is_some();
                    if self.active_list.as_ref() != Some(&context) {
                        self.prepare_list_context(&context);
                        self.active_list = Some(context.clone());
                    }
                    match items {
                        ListItems::Organizations(items) => {
                            extend_or_replace(&mut self.picker.organizations, items, append)
                        }
                        ListItems::Shared { projects, files } => {
                            extend_or_replace(&mut self.picker.projects, projects, append);
                            extend_or_replace(&mut self.picker.files, files, append);
                        }
                        ListItems::Workspaces(items) => {
                            extend_or_replace(&mut self.picker.workspaces, items, append)
                        }
                        ListItems::Projects(items) => {
                            extend_or_replace(&mut self.picker.projects, items, append)
                        }
                        ListItems::Files(items) => {
                            extend_or_replace(&mut self.picker.files, items, append)
                        }
                    }
                    self.next_cursor = next_cursor;
                    self.picker.more_available = self.next_cursor.is_some();
                    self.picker.dialog = Some(Dialog::Browse);
                }
                Event::Downloaded {
                    path, read_only, ..
                } => {
                    self.local_path = Some(path.clone());
                    self.selected_read_only = read_only;
                    self.status = format!("Verified download cached at {}", path.display());
                    self.picker.dialog = Some(Dialog::Browse);
                }
                Event::Saved(file_id) => {
                    self.picker.selected_file = Some(file_id);
                    self.status = if cancel_requested {
                        "Cloud save completed before the cancel request took effect; local file retained"
                    } else {
                        "Cloud save complete; local file retained"
                    }
                    .into();
                    self.picker.dialog = Some(Dialog::Save);
                }
                Event::Retryable { file_id, message } => {
                    self.picker.selected_file = Some(file_id);
                    self.picker.error = Some(message.clone());
                    self.status = message;
                    self.picker.dialog = Some(Dialog::Offline);
                }
                Event::SaveNewFailed { name, message } => {
                    self.picker.selected_file = None;
                    self.picker.file_name = name;
                    self.picker.error = Some(message.clone());
                    self.status = message;
                    self.picker.dialog = Some(Dialog::Save);
                }
                Event::Closed => {
                    self.selected_read_only = false;
                    self.status = "Cloud lease released".into();
                    self.picker.dialog = Some(Dialog::Browse);
                }
                Event::Conflict(id) => {
                    self.picker.selected_file = Some(id);
                    self.picker.pending_overwrite_confirmation = false;
                    self.picker.dialog = Some(Dialog::Conflict);
                }
                Event::Locked(owner) => {
                    self.picker.locked_by = Some(owner);
                    self.picker.dialog = Some(Dialog::Lock);
                }
                Event::LoggedOut | Event::LoggedOutLocally(_) => {
                    self.clear_server_context();
                    self.picker.email.clear();
                    self.picker.account_name.clear();
                    self.picker.organization_name.clear();
                    self.picker.requires_second_factor = false;
                    self.picker.second_factor_challenge_id = None;
                    self.picker.dialog = Some(Dialog::Login);
                    self.status = "Signed out".into();
                    if let Event::LoggedOutLocally(error) = event {
                        let message = format!(
                            "Signed out locally; the server session could not be ended ({error})"
                        );
                        self.picker.error = Some(message.clone());
                        self.status = message;
                    }
                }
                Event::Failed(error) => {
                    self.picker.error = Some(error.clone());
                    self.status = error;
                    if self.picker.dialog == Some(Dialog::Progress) {
                        self.picker.dialog = Some(Dialog::Offline);
                    }
                }
                Event::Cancelled => {
                    self.status = "Cloud action cancelled".into();
                    self.picker.dialog = None;
                }
                // Heartbeat events are handled above, independently of foreground jobs.
                Event::LeaseLost { .. } => {}
            }
            if let Some(file_id) = close_file_after_cancel {
                self.send(Request::CloseFile(file_id));
            }
            ctx.request_repaint();
        }
    }

    fn handle(&mut self, action: Action) {
        match action {
            Action::AddServer(server) => {
                if self.busy {
                    self.status = "Wait for the current cloud action to finish".into();
                    return;
                }
                self.clear_server_context();
                self.picker.email.clear();
                self.picker.account_name.clear();
                self.picker.server_url = server.clone();
                self.send(Request::Discover(server));
            }
            Action::Login {
                email,
                password,
                second_factor,
                ..
            } => match (
                self.picker.second_factor_challenge_id.clone(),
                second_factor,
            ) {
                (Some(challenge_id), Some(code)) => {
                    let proof = match classify_second_factor(&code) {
                        Some(SecondFactorInput::Totp(code)) => Some(Proof::totp(code)),
                        Some(SecondFactorInput::Recovery(code)) => Some(Proof::recovery(code)),
                        None => None,
                    };
                    if let Some(proof) = proof {
                        self.send(Request::SecondFactor {
                            email,
                            challenge_id,
                            proof,
                        });
                    } else {
                        self.status =
                            "Enter a six-digit code or a ten-character recovery code".into();
                    }
                }
                _ => self.send(Request::Login { email, password }),
            },
            Action::Logout => {
                self.clear_server_context();
                self.picker.account_name.clear();
                self.picker.email.clear();
                self.send(Request::Logout);
            }
            Action::SelectOrganization(id) => {
                self.picker.organization_name = self
                    .picker
                    .organizations
                    .iter()
                    .find(|(item_id, _)| item_id == &id)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default();
                self.clear_browse_descendants();
                self.organization_id = Some(id.clone());
                self.start_list(ListContext::Workspaces(id));
            }
            Action::SelectWorkspace(id) => {
                self.project_id = None;
                self.picker.projects.clear();
                self.picker.files.clear();
                self.picker.selected_file = None;
                self.workspace_id = Some(id.clone());
                self.start_list(ListContext::Projects(id));
            }
            Action::SelectProject(id) => {
                self.project_id = Some(id.clone());
                self.picker.files.clear();
                self.picker.selected_file = None;
                if self.keep_both_pending {
                    self.keep_both_pending = false;
                    self.picker.dialog = Some(Dialog::Save);
                    self.status = "Copy target selected. Save the retained local copy.".into();
                } else {
                    self.start_list(ListContext::Files(id));
                }
            }
            Action::OpenFile(file_id) => {
                self.selected_read_only = false;
                self.picker.selected_file = Some(file_id.clone());
                if let Some((_, name)) = self.picker.files.iter().find(|(id, _)| id == &file_id) {
                    self.file_name = name.clone();
                    // The save dialog edits `picker.file_name`; a name left over from an
                    // earlier copy would otherwise be offered for the newly opened file.
                    self.picker.file_name = name.clone();
                }
                self.send(Request::Open {
                    file_id,
                    read_only: false,
                });
            }
            Action::OpenShared => {
                if let Some(id) = self.organization_id.clone() {
                    self.clear_browse_descendants();
                    self.start_list(ListContext::Shared(id));
                } else {
                    self.status = "Select an organization first".into();
                }
            }
            Action::SaveNew { name } => {
                self.file_name = name.clone();
                if let (Some(path), Some(project_id)) =
                    (self.local_path.clone(), self.project_id.clone())
                {
                    self.picker.selected_file = None;
                    self.send(Request::SaveNew {
                        project_id,
                        name,
                        path,
                    });
                } else {
                    self.status = "Choose a project and local file first".into();
                }
            }
            Action::SaveVersion { file_id } => {
                if self.selected_read_only {
                    self.status = "Read-only files cannot be saved as a version".into();
                } else if let Some(path) = self.local_path.clone() {
                    self.send(Request::SaveVersion { file_id, path });
                } else {
                    self.status = "Choose a local file first".into();
                }
            }
            Action::KeepBoth => {
                self.keep_both_pending = true;
                self.project_id = None;
                self.picker.selected_file = None;
                self.picker.file_name = copy_name(&self.file_name, &self.picker.account_name);
                self.picker.dialog = Some(Dialog::Browse);
                self.status = "Choose a project for the retained local copy".into();
                self.clear_browse_descendants();
                if let Some(organization_id) = self.organization_id.clone() {
                    self.start_list(ListContext::Workspaces(organization_id));
                } else {
                    self.start_list(ListContext::Organizations);
                }
            }
            Action::Overwrite => {
                if self.selected_read_only {
                    self.status = "Read-only files cannot be overwritten".into();
                } else if let (Some(path), Some(file_id)) =
                    (self.local_path.clone(), self.picker.selected_file.clone())
                {
                    self.send(Request::Overwrite { file_id, path });
                }
            }
            Action::Cancel => {
                if let Some((_, cancelled)) = &self.current_job {
                    cancelled.store(true, Ordering::Release);
                    self.status = "Cancelling after current network step".into();
                } else {
                    self.keep_both_pending = false;
                    self.picker.dialog = None;
                    self.status = "Cancelled".into();
                }
            }
            Action::OpenReadOnly => {
                if let Some(file_id) = self.picker.selected_file.clone() {
                    self.selected_read_only = true;
                    self.send(Request::Open {
                        file_id,
                        read_only: true,
                    });
                }
            }
            Action::RetryUpload => {
                if let Some(path) = self.local_path.clone() {
                    if let Some(file_id) = self.picker.selected_file.clone() {
                        if self.selected_read_only {
                            self.status = "Read-only files cannot be saved as a version".into();
                        } else {
                            self.send(Request::SaveVersion { file_id, path });
                        }
                    } else if let Some(project_id) = self.project_id.clone() {
                        self.send(Request::SaveNew {
                            project_id,
                            name: self.file_name.clone(),
                            path,
                        });
                    }
                }
            }
            Action::More => self.load_more(),
        }
    }
}

impl Drop for DemoApp {
    fn drop(&mut self) {
        if let Some((_, cancelled)) = &self.current_job {
            cancelled.store(true, Ordering::Release);
        }
        let shutdown = Job {
            id: self.next_job_id,
            request: Request::Shutdown,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let _ = self.request.send(shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl eframe::App for DemoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.process_events(&ctx);
        egui::Panel::top("demo-controls").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("CraftCloud demo");
                if ui.button("EN").clicked() {
                    self.language = Language::English;
                }
                if ui.button("DE").clicked() {
                    self.language = Language::German;
                }
                if ui.button("Server").clicked() {
                    self.picker.dialog = Some(Dialog::Server);
                }
                if ui.button("Account").clicked() {
                    self.picker.dialog = Some(Dialog::Account);
                }
                if ui.button("Browse").clicked() {
                    self.picker.dialog = Some(Dialog::Browse);
                }
                if ui.button("Save").clicked() {
                    self.picker.dialog = Some(Dialog::Save);
                }
                if ui.button("Offline hint").clicked() {
                    self.picker.dialog = Some(Dialog::Offline);
                }
                if ui.button("Close cloud file").clicked()
                    && !self.busy
                    && let Some(id) = self.picker.selected_file.clone()
                {
                    self.send(Request::CloseFile(id));
                }
                if ui.button("Choose local file").clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    self.file_name = path
                        .file_name()
                        .map(|v| v.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    self.local_path = Some(path);
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if !self.status.is_empty() {
                ui.label(&self.status);
            }
            if let Some(path) = &self.local_path {
                ui.label(format!("Local file retained: {}", path.display()));
            }
            if self.busy {
                ui.spinner();
            }
        });
        let actions = self.picker.show(&ctx, &DefaultTranslator(self.language));
        for action in actions {
            self.handle(action);
        }
    }
}

fn worker(requests: Receiver<Job>, events: Sender<WorkerEvent>) {
    let mut client: Option<std::sync::Arc<CloudClient<UreqTransport>>> = None;
    let mut server = String::new();
    let mut leases: HashMap<Uuid, FileLeaseHeartbeat> = HashMap::new();
    let data_dir = match app_data_dir() {
        Ok(dir) => dir,
        Err(message) => {
            // Without a private data directory nothing may be stored: answer every
            // request with the reason instead of falling back to a shared location.
            while let Ok(job) = requests.recv() {
                let shutdown = matches!(&job.request, Request::Shutdown);
                let _ = events.send(WorkerEvent {
                    id: Some(job.id),
                    event: if shutdown {
                        Event::Closed
                    } else {
                        Event::Failed(message.clone())
                    },
                });
                if shutdown {
                    break;
                }
            }
            return;
        }
    };
    let cache = VersionCache::new(data_dir.join("CraftCloud"));
    let keychain = KeychainTokenStore::new("craftcloud");
    let links = LinksStore::new(&data_dir);
    while let Ok(job) = requests.recv() {
        let shutdown = matches!(&job.request, Request::Shutdown);
        let requested_open_file = match &job.request {
            Request::Open { file_id, .. } => Uuid::parse_str(file_id).ok(),
            _ => None,
        };
        let result: Result<Event, String> = if job.cancelled.load(Ordering::Acquire) {
            Err("cancelled".into())
        } else {
            match job.request {
                Request::Discover(base) => {
                    let canonical = canonical_server(&base);
                    if server != canonical {
                        // Release the leases first: they hold the old client and need
                        // its session to unlock the files.
                        leases.clear();
                        if let Some(previous) = client.take()
                            && previous.is_authenticated()
                        {
                            let _ = previous.logout();
                        }
                        server.clear();
                    }
                    match CloudClient::new(canonical.clone(), UreqTransport::default()) {
                        Ok(candidate) => match candidate.discover() {
                            Ok(_) => {
                                // A reconnect replaces the client. The old one must not
                                // stay behind holding leases or a stale copy of the
                                // refresh token that the new client shares through the
                                // keychain.
                                leases.clear();
                                drop(client.take());
                                server = canonical;
                                client = Some(std::sync::Arc::new(candidate));
                                Ok(Event::Discovered)
                            }
                            Err(error) => Err(error.to_string()),
                        },
                        Err(error) => Err(error.to_string()),
                    }
                }
                Request::Login { email, password } => {
                    if let Some(active) = client.as_ref() {
                        if active
                            .authenticated_user()
                            .is_some_and(|user| account_key(&user.email) != account_key(&email))
                        {
                            let _ = active.logout();
                        }
                        let bound = bind_store(&keychain, &data_dir, &server, &email);
                        let stored = bound.load().ok().flatten();
                        match active.set_refresh_token_store(Arc::new(bound)) {
                            Err(error) => Err(error.to_string()),
                            Ok(()) => {
                                let restored_event = if let Some(token) = stored {
                                    active.restore_refresh_token(token);
                                    if active.refresh().is_ok() {
                                        Some(login_event(active, &email))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                                if let Some(event) = restored_event {
                                    Ok(event)
                                } else {
                                    match active.login(
                                        &email,
                                        &password,
                                        "app",
                                        "craftcloud-egui-demo",
                                        "CraftCloud egui demo",
                                    ) {
                                        Ok(()) => Ok(login_event(active, &email)),
                                        Err(CloudError::SecondFactorChallenge(id)) => {
                                            Ok(Event::SecondFactor(id))
                                        }
                                        Err(error) => Err(error.to_string()),
                                    }
                                }
                            }
                        }
                    } else {
                        Err("Add and discover a server first".into())
                    }
                }
                Request::SecondFactor {
                    email,
                    challenge_id,
                    proof,
                } => {
                    if let Some(active) = client.as_ref() {
                        let bound = bind_store(&keychain, &data_dir, &server, &email);
                        match active.set_refresh_token_store(Arc::new(bound)) {
                            Err(error) => Err(error.to_string()),
                            Ok(()) => Ok(match active.complete_second_factor(&challenge_id, proof) {
                                Ok(()) => login_event(active, &email),
                                Err(CloudError::SecondFactorChallenge(id)) => {
                                    Event::SecondFactor(id)
                                }
                                Err(CloudError::Unauthorized) => Event::SecondFactorRejected(
                                    "The code was not accepted or the sign-in expired. Check the code or cancel and sign in again".into(),
                                ),
                                Err(error) => Event::SecondFactorRejected(error.to_string()),
                            }),
                        }
                    } else {
                        Err("Add and discover a server first".into())
                    }
                }
                Request::List { context, cursor } => client
                    .as_ref()
                    .ok_or_else(|| "Sign in first".to_string())
                    .and_then(|c| {
                        fetch_list(c, &context, cursor.as_deref()).map(|(items, next_cursor)| {
                            Event::Listing {
                                context,
                                items,
                                next_cursor,
                            }
                        })
                    }),
                Request::Open { file_id, read_only } => {
                    leases.clear();
                    open_file(
                        OpenResources {
                            client: client.as_ref(),
                            cache: &cache,
                            links: &links,
                            server: &server,
                            leases: &mut leases,
                            events: &events,
                        },
                        &file_id,
                        read_only,
                        &job.cancelled,
                    )
                    .map_err(|e| e.to_string())
                }
                Request::SaveNew {
                    project_id,
                    name,
                    path,
                } => {
                    let previous_link = links.get(&path);
                    let previous_file_id = previous_link
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .map(|link| link.file_id.clone());
                    match save_new(
                        client.as_ref(),
                        &links,
                        &server,
                        &project_id,
                        &name,
                        &path,
                        &job.cancelled,
                    ) {
                        Ok(Event::Saved(file_id)) => {
                            if previous_file_id
                                .as_deref()
                                .is_some_and(|old_file_id| old_file_id != file_id)
                                && let Some(old_file_id) = previous_file_id.as_deref()
                                && let Ok(old_id) = Uuid::parse_str(old_file_id)
                            {
                                leases.remove(&old_id);
                            }
                            Ok(Event::Saved(file_id))
                        }
                        Ok(event) => Ok(event),
                        Err(error) => Ok(classify_new_save_failure(
                            name,
                            error,
                            previous_link,
                            links.get(&path),
                        )),
                    }
                }
                Request::SaveVersion { file_id, path } => save_version(
                    client.as_ref(),
                    &links,
                    &server,
                    &file_id,
                    &path,
                    &job.cancelled,
                )
                .map_err(|e| e.to_string()),
                Request::Overwrite { file_id, path } => overwrite(
                    client.as_ref(),
                    &links,
                    &server,
                    &file_id,
                    &path,
                    &job.cancelled,
                )
                .map_err(|e| e.to_string()),
                Request::Logout => {
                    leases.clear();
                    match client.as_ref().map(|active| active.logout()) {
                        None | Some(Ok(())) | Some(Err(CloudError::Unauthorized)) => {
                            Ok(Event::LoggedOut)
                        }
                        // The local session is gone either way; tell the user that the
                        // server may still hold it until it expires.
                        Some(Err(error)) => Ok(Event::LoggedOutLocally(error.to_string())),
                    }
                }
                Request::CloseFile(id) => match Uuid::parse_str(&id) {
                    Ok(uuid) => {
                        leases.remove(&uuid);
                        Ok(Event::Closed)
                    }
                    Err(_) => Err("Invalid file ID".into()),
                },
                Request::Shutdown => {
                    leases.clear();
                    Ok(Event::Closed)
                }
            }
        };
        let event = if job.cancelled.load(Ordering::Acquire) {
            match result {
                // The server already acted; report that instead of "cancelled".
                Ok(event) if survives_cancel(&event) => event,
                _ => {
                    if let Some(file_id) = requested_open_file {
                        leases.remove(&file_id);
                    }
                    Event::Cancelled
                }
            }
        } else {
            result.unwrap_or_else(Event::Failed)
        };
        let _ = events.send(WorkerEvent {
            id: Some(job.id),
            event,
        });
        if shutdown {
            break;
        }
    }
}

fn authenticated_profile(
    client: &CloudClient<UreqTransport>,
    fallback_email: &str,
) -> (String, String) {
    let user = client
        .authenticated_user()
        .or_else(|| client.current_user().ok());
    user.map(|user| (user.display_name, user.email))
        .unwrap_or_else(|| (fallback_email.to_owned(), fallback_email.to_owned()))
}

fn login_event(client: &CloudClient<UreqTransport>, fallback_email: &str) -> Event {
    let (display_name, email) = authenticated_profile(client, fallback_email);
    Event::LoggedIn {
        display_name,
        email,
    }
}

/// The refresh-token slot of one account on one server, with a lock file in the app data
/// directory so that several processes sharing the slot never rotate the token at once.
fn bind_store(
    keychain: &KeychainTokenStore,
    data_dir: &Path,
    server: &str,
    email: &str,
) -> craftcloud_client_native::BoundKeychainTokenStore {
    keychain
        .bound(
            server.to_owned(),
            account_key(email),
            "craftcloud-egui-demo",
        )
        .with_lock_dir(data_dir)
}

fn canonical_server(server: &str) -> String {
    Url::parse(server)
        .map(|url| url.as_str().trim_end_matches('/').to_owned())
        .unwrap_or_else(|_| server.to_owned())
}

fn account_key(email: &str) -> String {
    email.trim().to_lowercase()
}

fn fetch_list(
    client: &CloudClient<UreqTransport>,
    context: &ListContext,
    cursor: Option<&str>,
) -> Result<(ListItems, Option<String>), String> {
    match context {
        ListContext::Organizations => {
            let (items, next) =
                fetch_pages::<Organization>(client, "/api/v1/organizations", cursor)?;
            Ok((
                ListItems::Organizations(
                    items
                        .into_iter()
                        .map(|item| (item.id.to_string(), item.name))
                        .collect(),
                ),
                next,
            ))
        }
        ListContext::Shared(org_id) => {
            let (items, next) = fetch_pages::<SharedItem>(
                client,
                &format!("/api/v1/organizations/{org_id}/shared-with-me"),
                cursor,
            )?;
            let mut projects = Vec::new();
            let mut files = Vec::new();
            for item in items {
                if let Some(project) = item.project {
                    projects.push((project.id, project.name));
                }
                if let Some(file) = item.file {
                    files.push((file.id, file.name));
                }
            }
            Ok((ListItems::Shared { projects, files }, next))
        }
        ListContext::Workspaces(org_id) => {
            let (items, next) = fetch_pages::<Workspace>(
                client,
                &format!("/api/v1/organizations/{org_id}/workspaces"),
                cursor,
            )?;
            Ok((
                ListItems::Workspaces(
                    items
                        .into_iter()
                        .map(|item| (item.id.to_string(), item.name))
                        .collect(),
                ),
                next,
            ))
        }
        ListContext::Projects(workspace_id) => {
            let (items, next) = fetch_pages::<Project>(
                client,
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
                cursor,
            )?;
            Ok((
                ListItems::Projects(
                    items
                        .into_iter()
                        .map(|item| (item.id.to_string(), item.name))
                        .collect(),
                ),
                next,
            ))
        }
        ListContext::Files(project_id) => {
            let (items, next) = fetch_pages::<CloudFile>(
                client,
                &format!("/api/v1/projects/{project_id}/files"),
                cursor,
            )?;
            Ok((
                ListItems::Files(
                    items
                        .into_iter()
                        .map(|item| (item.id.to_string(), item.name))
                        .collect(),
                ),
                next,
            ))
        }
    }
}

fn fetch_pages<T: for<'de> Deserialize<'de>>(
    client: &CloudClient<UreqTransport>,
    path: &str,
    cursor: Option<&str>,
) -> Result<(Vec<T>, Option<String>), String> {
    let first: Page<T> = client
        .get(&page_path(path, cursor))
        .map_err(|error| error.to_string())?;
    collect_page_batch(first, |next| {
        client
            .get(&page_path(path, Some(next)))
            .map_err(|error| error.to_string())
    })
}

fn page_path(path: &str, cursor: Option<&str>) -> String {
    match cursor {
        Some(cursor) => {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("cursor", cursor)
                .finish();
            format!("{path}?{query}")
        }
        None => path.to_owned(),
    }
}

fn collect_page_batch<T>(
    first: Page<T>,
    mut fetch_next: impl FnMut(&str) -> Result<Page<T>, String>,
) -> Result<(Vec<T>, Option<String>), String> {
    let mut items = Vec::new();
    let mut page = first;
    for page_number in 0..PAGE_BATCH_SIZE {
        items.extend(page.items);
        let Some(cursor) = page.next_cursor else {
            return Ok((items, None));
        };
        if page_number + 1 == PAGE_BATCH_SIZE {
            return Ok((items, Some(cursor)));
        }
        page = fetch_next(&cursor)?;
    }
    Ok((items, None))
}

fn extend_or_replace(
    target: &mut Vec<(String, String)>,
    values: Vec<(String, String)>,
    append: bool,
) {
    if append {
        target.extend(values);
    } else {
        *target = values;
    }
}

struct OpenResources<'a> {
    client: Option<&'a Arc<CloudClient<UreqTransport>>>,
    cache: &'a VersionCache,
    links: &'a LinksStore,
    server: &'a str,
    leases: &'a mut HashMap<Uuid, FileLeaseHeartbeat>,
    events: &'a Sender<WorkerEvent>,
}

fn open_file(
    resources: OpenResources<'_>,
    id: &str,
    read_only: bool,
    cancelled: &Arc<AtomicBool>,
) -> Result<Event, String> {
    let OpenResources {
        client,
        cache,
        links,
        server,
        leases,
        events,
    } = resources;
    check_cancelled(cancelled)?;
    let active = client.ok_or_else(|| "Sign in first".to_string())?;
    let file_id = Uuid::parse_str(id).map_err(|_| "Invalid file ID".to_string())?;
    let file: CloudFile = active
        .get(&format!("/api/v1/files/{id}"))
        .map_err(|error| error.to_string())?;
    let mut lease = None;
    if !read_only && file.app != "figcraft" {
        // An earlier lease on this file may still be releasing (the heartbeat thread does
        // that after its current call). Acquiring first would let that release drop the
        // new lease.
        if !wait_for_release(id, std::time::Duration::from_secs(5)) {
            return Err("The previous cloud lease on this file is still being released; try again in a moment".into());
        }
        match active.acquire_lock(file_id, "craftcloud-egui-demo") {
            Ok(_) => {
                let heartbeat_client = std::sync::Arc::clone(active);
                let release_client = std::sync::Arc::clone(active);
                let notification_sender = events.clone();
                let notification_file_id = file_id.to_string();
                lease = Some(FileLeaseHeartbeat::start_keyed(
                    file_id.to_string(),
                    move || {
                        if let Err(error) =
                            heartbeat_client.acquire_lock(file_id, "craftcloud-egui-demo")
                        {
                            let _ = notification_sender.send(WorkerEvent {
                                id: None,
                                event: Event::LeaseLost {
                                    file_id: notification_file_id.clone(),
                                    message: error.to_string(),
                                },
                            });
                        }
                    },
                    move || {
                        let _ = release_client.release_lock(file_id);
                    },
                ));
            }
            Err(CloudError::FileLocked(owner)) => {
                return Ok(Event::Locked(
                    owner.unwrap_or_else(|| "another user".into()),
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let version_id = file
        .head_version_id
        .ok_or_else(|| "File has no committed version".to_string())?;
    let bytes = active
        .download(file_id, Some(version_id))
        .map_err(|error| error.to_string())?;
    check_cancelled(cancelled)?;
    let actual_digest = hex_digest(&bytes);
    if file
        .sha256
        .as_deref()
        .is_some_and(|expected| !expected.eq_ignore_ascii_case(&actual_digest))
    {
        return Err("Downloaded file checksum does not match the committed version".into());
    }
    // The registered format names the extension; a display name such as
    // "Logo v2.1 final" has no usable one.
    let extension = cache_extension(&file.format);
    let path = cache
        .store(server, id, &version_id.to_string(), &extension, &bytes)
        .map_err(|error| error.to_string())?;
    if !read_only {
        links
            .upsert(CloudLink {
                local_path: path.clone(),
                server: server.to_owned(),
                file_id: id.to_owned(),
                base_version: Some(version_id.to_string()),
                sha256: actual_digest,
            })
            .map_err(|error| error.to_string())?;
    }
    check_cancelled(cancelled)?;
    if let Some(lease) = lease {
        leases.insert(file_id, lease);
    }
    Ok(Event::Downloaded {
        file_id: id.to_owned(),
        path,
        read_only,
    })
}

const MAX_LOCAL_FILE_SIZE: usize = 128 * 1024 * 1024;

fn read_bounded(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let read_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::with_capacity(limit.min(1024 * 1024));
    file.take(read_limit).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Local file exceeds the {limit}-byte upload limit"),
        ));
    }
    Ok(bytes)
}

fn save_new(
    client: Option<&std::sync::Arc<CloudClient<UreqTransport>>>,
    links: &LinksStore,
    server: &str,
    project_id: &str,
    name: &str,
    path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> Result<Event, CloudError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(CloudError::Cancelled);
    }
    let active = client.ok_or(CloudError::Unauthorized)?;
    let bytes = read_bounded(path, MAX_LOCAL_FILE_SIZE)
        .map_err(|error| CloudError::Io(error.to_string()))?;
    let format = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "bin".into());
    let app = match format.as_str() {
        "pcraft" | "psd" | "psb" | "ora" => "photocraft",
        "figcraft" | "fig" => "figcraft",
        "svg" | "ai" | "vcraft" => "vectorcraft",
        _ => "generic",
    };
    let project = Uuid::parse_str(project_id).map_err(|_| CloudError::InvalidUrl)?;
    let target = NewFile {
        project_id: project,
        name: name.to_owned(),
        app: app.to_owned(),
        format,
        idempotency_key: Uuid::new_v4().to_string(),
    };
    let _version = save_new_local_first_with_cancel(
        active,
        links,
        path,
        server,
        &target,
        bytes,
        Arc::clone(cancelled),
    )?;
    let link = links.get(path)?.ok_or(CloudError::InvalidResponse)?;
    Ok(Event::Saved(link.file_id))
}

fn classify_new_save_failure(
    name: String,
    error: CloudError,
    previous_link: Result<Option<CloudLink>, CloudError>,
    current_link: Result<Option<CloudLink>, CloudError>,
) -> Event {
    let created_link = match (previous_link, current_link) {
        (Ok(previous), Ok(Some(current)))
            if previous
                .as_ref()
                .is_none_or(|previous| previous.file_id != current.file_id) =>
        {
            Some(current)
        }
        _ => None,
    };
    if let Some(link) = created_link {
        if matches!(&error, CloudError::VersionConflict(_)) {
            Event::Conflict(link.file_id)
        } else {
            Event::Retryable {
                file_id: link.file_id,
                message: error.to_string(),
            }
        }
    } else {
        Event::SaveNewFailed {
            name,
            message: error.to_string(),
        }
    }
}

fn save_version(
    client: Option<&std::sync::Arc<CloudClient<UreqTransport>>>,
    links: &LinksStore,
    server: &str,
    file_id: &str,
    path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> Result<Event, String> {
    check_cancelled(cancelled)?;
    let active = client.ok_or_else(|| "Sign in first".to_string())?;
    let id = Uuid::parse_str(file_id).map_err(|_| "Invalid file ID".to_string())?;
    let link = links
        .get(path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| {
            "Local file has no CraftCloud link; save it as a new file first".to_string()
        })?;
    if link.server != server || link.file_id != file_id {
        return Err("Local file is linked to a different CraftCloud file".into());
    }
    let bytes = read_bounded(path, MAX_LOCAL_FILE_SIZE).map_err(|error| error.to_string())?;
    let upload = UploadContent {
        file_id: id,
        base_version: None,
        idempotency_key: Uuid::new_v4().to_string(),
        bytes,
    };
    match save_local_first_with_cancel(active, links, path, server, upload, Arc::clone(cancelled)) {
        Ok(SaveOutcome::Uploaded(_)) => Ok(Event::Saved(id.to_string())),
        Ok(SaveOutcome::LocalOnly) => Ok(Event::Failed(
            "Local file is not linked to CraftCloud".into(),
        )),
        Err(CloudError::VersionConflict(_)) => Ok(Event::Conflict(file_id.into())),
        Err(error) => Err(error.to_string()),
    }
}

fn overwrite(
    client: Option<&std::sync::Arc<CloudClient<UreqTransport>>>,
    links: &LinksStore,
    server: &str,
    file_id: &str,
    path: &Path,
    cancelled: &Arc<AtomicBool>,
) -> Result<Event, String> {
    check_cancelled(cancelled)?;
    let active = client.ok_or_else(|| "Sign in first".to_string())?;
    let id = Uuid::parse_str(file_id).map_err(|_| "Invalid file ID".to_string())?;
    let link = links
        .get(path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Local file has no CraftCloud link".to_string())?;
    if link.server != server || link.file_id != file_id {
        return Err("Local file is linked to a different CraftCloud file".into());
    }
    let bytes = read_bounded(path, MAX_LOCAL_FILE_SIZE).map_err(|error| error.to_string())?;
    let file: CloudFile = active
        .get(&format!("/api/v1/files/{id}"))
        .map_err(|error| error.to_string())?;
    let upload = UploadContent {
        file_id: id,
        base_version: None,
        idempotency_key: Uuid::new_v4().to_string(),
        bytes,
    };
    // The current head is the base for this upload only. The link keeps its old base
    // until the commit succeeded, so a failed or cancelled overwrite cannot let a later
    // plain save pass the conflict check.
    match overwrite_local_first_with_cancel(
        active,
        links,
        path,
        server,
        upload,
        file.head_version_id,
        Arc::clone(cancelled),
    ) {
        Ok(SaveOutcome::Uploaded(_)) => Ok(Event::Saved(id.to_string())),
        Ok(SaveOutcome::LocalOnly) => Ok(Event::Failed(
            "Local file is not linked to CraftCloud".into(),
        )),
        Err(CloudError::VersionConflict(_)) => Ok(Event::Conflict(file_id.into())),
        Err(error) => Err(error.to_string()),
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("cancelled".into())
    } else {
        Ok(())
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Where the demo keeps links, cache and lock files. Always a per-user directory: there is
/// no fallback to a shared temporary directory, where another user could plant or lock
/// files. `CRAFTCLOUD_DEMO_DATA_DIR` overrides it (for isolated runs).
fn app_data_dir() -> Result<PathBuf, String> {
    let platform = if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Unix
    };
    resolve_data_dir(platform, |name| std::env::var_os(name))
}

#[derive(Clone, Copy)]
enum Platform {
    Windows,
    MacOs,
    Unix,
}

fn resolve_data_dir(
    platform: Platform,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PathBuf, String> {
    let absolute = |name: &str| {
        env(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    if let Some(path) = env("CRAFTCLOUD_DEMO_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let base = match platform {
        Platform::Windows => absolute("LOCALAPPDATA").or_else(|| absolute("APPDATA")),
        Platform::MacOs => {
            absolute("HOME").map(|home| home.join("Library").join("Application Support"))
        }
        Platform::Unix => absolute("XDG_DATA_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".local").join("share"))),
    };
    base.map(|dir| dir.join("CraftCloudDemo")).ok_or_else(|| {
        "No per-user data directory found; set CRAFTCLOUD_DEMO_DATA_DIR to a private folder"
            .to_owned()
    })
}

fn copy_name(name: &str, person: &str) -> String {
    let now = time::OffsetDateTime::now_utc();
    let date = format!(
        "{}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    );
    match name.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem} (Kopie von {person}, {date}).{extension}"),
        None => format!("{name} (Kopie von {person}, {date})"),
    }
}

fn main() -> eframe::Result {
    let server = std::env::var("CRAFTCLOUD_DEMO_SERVER").unwrap_or_default();
    eframe::run_native(
        "CraftCloud egui demo",
        eframe::NativeOptions::default(),
        Box::new(move |_cc| Ok(Box::new(DemoApp::new(server)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> (DemoApp, Receiver<Job>, Sender<WorkerEvent>) {
        let (request, requests) = mpsc::channel();
        let (event_sender, events) = mpsc::channel();
        let app = DemoApp {
            picker: PickerState::default(),
            language: Language::English,
            request,
            events,
            busy: false,
            status: String::new(),
            project_id: None,
            organization_id: None,
            workspace_id: None,
            active_list: None,
            next_cursor: None,
            keep_both_pending: false,
            file_name: String::new(),
            local_path: None,
            selected_read_only: false,
            demo_credentials: None,
            current_job: None,
            next_job_id: 0,
            worker: None,
        };
        (app, requests, event_sender)
    }

    #[test]
    fn organization_and_workspace_switches_clear_child_target_state() {
        let (mut app, _requests, _events) = test_app();
        app.organization_id = Some("org-old".into());
        app.workspace_id = Some("workspace-old".into());
        app.project_id = Some("project-old".into());
        app.picker.organizations = vec![("org-new".into(), "New org".into())];
        app.picker.workspaces = vec![("workspace-old".into(), "Old workspace".into())];
        app.picker.projects = vec![("project-old".into(), "Old project".into())];
        app.picker.files = vec![("file-old".into(), "Old file".into())];
        app.picker.selected_file = Some("file-old".into());

        app.handle(Action::SelectOrganization("org-new".into()));

        assert_eq!(app.organization_id.as_deref(), Some("org-new"));
        assert_eq!(app.workspace_id, None);
        assert_eq!(app.project_id, None);
        assert!(app.picker.workspaces.is_empty());
        assert!(app.picker.projects.is_empty());
        assert!(app.picker.files.is_empty());
        assert_eq!(app.picker.selected_file, None);

        app.busy = false;
        app.current_job = None;
        app.picker.projects = vec![("project-stale".into(), "Stale project".into())];
        app.picker.files = vec![("file-stale".into(), "Stale file".into())];
        app.picker.selected_file = Some("file-stale".into());
        app.handle(Action::SelectWorkspace("workspace-new".into()));

        assert_eq!(app.workspace_id.as_deref(), Some("workspace-new"));
        assert_eq!(app.project_id, None);
        assert!(app.picker.projects.is_empty());
        assert!(app.picker.files.is_empty());
        assert_eq!(app.picker.selected_file, None);
    }

    #[test]
    fn opening_another_file_replaces_the_visible_file_name() {
        let (mut app, _requests, _events) = test_app();
        app.picker.files = vec![
            ("original".into(), "Datei.svg".into()),
            (
                "copy".into(),
                "Datei (Kopie von SDK Demo, 2026-10-30) 2.svg".into(),
            ),
        ];
        app.handle(Action::OpenFile("copy".into()));
        assert_eq!(
            app.picker.file_name,
            "Datei (Kopie von SDK Demo, 2026-10-30) 2.svg"
        );
        app.busy = false;
        app.current_job = None;
        app.handle(Action::OpenFile("original".into()));
        assert_eq!(app.picker.selected_file.as_deref(), Some("original"));
        assert_eq!(app.picker.file_name, "Datei.svg");
    }

    #[test]
    fn server_change_and_logout_clear_account_target_context() {
        let (mut app, _requests, _events) = test_app();
        app.picker.organizations = vec![("org".into(), "Organization".into())];
        app.organization_id = Some("org".into());
        app.workspace_id = Some("workspace".into());
        app.project_id = Some("project".into());
        app.picker.selected_file = Some("file".into());
        app.picker.file_name = "stale.pcraft".into();
        app.picker.files = vec![("file".into(), "stale.pcraft".into())];
        app.picker.email = "old@example.test".into();

        app.handle(Action::AddServer("https://new.example".into()));
        assert_eq!(app.picker.server_url, "https://new.example");
        assert_eq!(app.organization_id, None);
        assert_eq!(app.workspace_id, None);
        assert_eq!(app.project_id, None);
        assert_eq!(app.picker.selected_file, None);
        assert!(app.picker.organizations.is_empty());
        assert!(app.picker.files.is_empty());
        assert!(app.picker.file_name.is_empty());
        assert!(app.picker.email.is_empty());

        app.busy = false;
        app.current_job = None;
        app.picker.organizations = vec![("org".into(), "Organization".into())];
        app.organization_id = Some("org".into());
        app.project_id = Some("project".into());
        app.picker.selected_file = Some("file".into());
        app.handle(Action::Logout);
        assert_eq!(app.organization_id, None);
        assert_eq!(app.project_id, None);
        assert_eq!(app.picker.selected_file, None);
        assert!(app.picker.organizations.is_empty());
    }

    #[test]
    fn downloaded_file_becomes_active_local_document() {
        let (mut app, _requests, event_sender) = test_app();
        let path = PathBuf::from("cache/project/version.svg");
        app.current_job = Some((4, Arc::new(AtomicBool::new(false))));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(4),
                event: Event::Downloaded {
                    file_id: "file-id".into(),
                    path: path.clone(),
                    read_only: false,
                },
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert_eq!(app.local_path, Some(path));
        assert!(!app.selected_read_only);
    }

    #[test]
    fn keep_both_without_target_preserves_local_copy_and_requests_browse() {
        let (mut app, requests, events) = test_app();
        let local = PathBuf::from("cache/shared/file.svg");
        app.local_path = Some(local.clone());
        app.file_name = "file.svg".into();
        app.picker.account_name = "Sample Person".into();
        app.picker.selected_file = Some("shared-file".into());

        app.handle(Action::KeepBoth);

        assert!(app.keep_both_pending);
        assert_eq!(app.project_id, None);
        assert_eq!(app.local_path, Some(local));
        assert!(
            app.picker
                .file_name
                .starts_with("file (Kopie von Sample Person, ")
        );
        assert_eq!(app.picker.dialog, Some(Dialog::Progress));
        let job = requests.try_recv();
        assert!(matches!(
            job.as_ref().map(|job| &job.request),
            Ok(Request::List {
                context: ListContext::Organizations,
                cursor: None
            })
        ));
        if let Ok(job) = job {
            assert!(
                events
                    .send(WorkerEvent {
                        id: Some(job.id),
                        event: Event::Listing {
                            context: ListContext::Organizations,
                            items: ListItems::Organizations(vec![(
                                "org".into(),
                                "Copy destination".into()
                            )]),
                            next_cursor: None,
                        }
                    })
                    .is_ok()
            );
        }
        app.process_events(&egui::Context::default());
        assert_eq!(app.picker.dialog, Some(Dialog::Browse));
        assert!(app.keep_both_pending);
    }

    #[test]
    fn save_new_collision_with_existing_link_never_retries_original_file() {
        let existing = CloudLink {
            local_path: PathBuf::from("cache/original.svg"),
            server: "https://cloud.example".into(),
            file_id: "original-file-id".into(),
            base_version: Some("base-version".into()),
            sha256: "digest".into(),
        };
        let event = classify_new_save_failure(
            "original (Kopie von Alice, 2026-10-30).svg".into(),
            CloudError::Server {
                status: 409,
                code: "FILE_NAME_TAKEN".into(),
                message: "Name already exists".into(),
            },
            Ok(Some(existing.clone())),
            Ok(Some(existing)),
        );

        assert!(matches!(
            event,
            Event::SaveNewFailed { name, .. }
                if name == "original (Kopie von Alice, 2026-10-30).svg"
        ));
    }

    #[test]
    fn save_new_failure_keeps_copy_name_editable_and_retries_as_new_file() {
        let (mut app, requests, event_sender) = test_app();
        let copy_name = "original (Kopie von Alice, 2026-10-30).svg".to_owned();
        app.current_job = Some((22, Arc::new(AtomicBool::new(false))));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(22),
                event: Event::SaveNewFailed {
                    name: copy_name.clone(),
                    message: "FILE_NAME_TAKEN".into(),
                },
            })
            .ok();
        app.process_events(&egui::Context::default());

        assert_eq!(app.picker.dialog, Some(Dialog::Save));
        assert_eq!(app.picker.file_name, copy_name);
        assert_eq!(app.picker.selected_file, None);
        app.local_path = Some(PathBuf::from("cache/local-copy.svg"));
        app.project_id = Some("target-project".into());
        app.handle(Action::SaveNew {
            name: app.picker.file_name.clone(),
        });
        assert!(matches!(
            requests.try_recv().map(|job| job.request),
            Ok(Request::SaveNew { name, .. }) if name == "original (Kopie von Alice, 2026-10-30).svg"
        ));
    }

    #[test]
    fn selecting_explicit_copy_target_retains_copy_intent() {
        let (mut app, _requests, _events) = test_app();
        app.keep_both_pending = true;
        app.local_path = Some(PathBuf::from("cache/shared/file.svg"));
        app.picker.file_name = "file (Kopie von Sample Person, 2026-10-30).svg".into();
        app.picker.projects = vec![("target-project".into(), "Target".into())];
        app.handle(Action::SelectProject("target-project".into()));
        assert_eq!(app.project_id.as_deref(), Some("target-project"));
        assert_eq!(app.picker.dialog, Some(Dialog::Save));
        assert_eq!(
            app.picker.file_name,
            "file (Kopie von Sample Person, 2026-10-30).svg"
        );
        assert!(app.local_path.is_some());
        assert!(!app.keep_both_pending);
    }

    #[test]
    fn pagination_fetches_more_than_one_page_and_exposes_cap_cursor() {
        let first = Page {
            items: (0..50).collect::<Vec<_>>(),
            next_cursor: Some("next-1".into()),
        };
        let first_result = collect_page_batch(first, |cursor| {
            assert_eq!(cursor, "next-1");
            Ok(Page {
                items: vec![50],
                next_cursor: None,
            })
        });
        assert!(first_result.is_ok());
        let (items, next) = match first_result {
            Ok(page) => page,
            Err(_) => return,
        };
        assert_eq!(items.len(), 51);
        assert_eq!(next, None);

        let first = Page {
            items: vec![0; 50],
            next_cursor: Some("page-1".into()),
        };
        let mut calls = 0;
        let capped_result = collect_page_batch(first, |_cursor| {
            calls += 1;
            Ok(Page {
                items: vec![calls; 50],
                next_cursor: Some(format!("page-{}", calls + 1)),
            })
        });
        assert!(capped_result.is_ok());
        let (items, next) = match capped_result {
            Ok(page) => page,
            Err(_) => return,
        };
        assert_eq!(items.len(), PAGE_BATCH_SIZE * 50);
        assert_eq!(calls, PAGE_BATCH_SIZE - 1);
        assert_eq!(next.as_deref(), Some("page-10"));
    }

    #[test]
    fn cancelled_download_completion_queues_lease_release_by_event_file_id() {
        let (mut app, requests, event_sender) = test_app();
        let cancelled = Arc::new(AtomicBool::new(false));
        app.current_job = Some((12, Arc::clone(&cancelled)));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(12),
                event: Event::Downloaded {
                    file_id: "downloaded-file-id".into(),
                    path: PathBuf::from("cache/download.svg"),
                    read_only: false,
                },
            })
            .ok();
        cancelled.store(true, Ordering::Release);

        app.process_events(&egui::Context::default());

        assert!(matches!(
            requests.try_recv().map(|job| job.request),
            Ok(Request::CloseFile(file_id)) if file_id == "downloaded-file-id"
        ));
    }

    #[test]
    fn bounded_file_read_rejects_oversize_without_large_allocation() {
        let path =
            std::env::temp_dir().join(format!("craftcloud-read-limit-{}.tmp", Uuid::new_v4()));
        let missing_path = path.with_extension("missing");
        let write_result = std::fs::write(&path, b"12345");
        assert!(write_result.is_ok());
        let read_result = read_bounded(&path, 4);
        let _ = std::fs::remove_file(&path);
        assert!(matches!(read_result, Err(error) if error.kind() == io::ErrorKind::InvalidData));
        assert_eq!(
            read_bounded(&missing_path, 4)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn a_cancel_after_the_save_finished_keeps_the_saved_file() {
        let (mut app, _requests, event_sender) = test_app();
        let cancelled = Arc::new(AtomicBool::new(true));
        app.current_job = Some((31, cancelled));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(31),
                event: Event::Saved("created-file".into()),
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert_eq!(app.picker.selected_file.as_deref(), Some("created-file"));
        assert!(
            app.status.contains("completed before the cancel"),
            "{}",
            app.status
        );

        // A file that was created but not yet uploaded stays selected for the retry.
        app.current_job = Some((32, Arc::new(AtomicBool::new(true))));
        app.busy = true;
        app.picker.selected_file = None;
        event_sender
            .send(WorkerEvent {
                id: Some(32),
                event: Event::Retryable {
                    file_id: "created-file".into(),
                    message: "Operation cancelled".into(),
                },
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert_eq!(app.picker.selected_file.as_deref(), Some("created-file"));

        // Other results are still replaced by the cancellation.
        app.current_job = Some((33, Arc::new(AtomicBool::new(true))));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(33),
                event: Event::Failed("late failure".into()),
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert_eq!(app.status, "Cloud action cancelled");
    }

    #[test]
    fn second_factor_codes_are_sent_as_the_matching_proof() {
        let (mut app, requests, _events) = test_app();
        app.picker.second_factor_challenge_id = Some("challenge-1".into());
        app.handle(Action::Login {
            server: "https://cloud.example".into(),
            email: "a@example.test".into(),
            password: String::new(),
            second_factor: Some("123456".into()),
        });
        assert!(matches!(
            requests.try_recv().map(|job| job.request),
            Ok(Request::SecondFactor { challenge_id, proof: Proof::Totp(_), .. })
                if challenge_id == "challenge-1"
        ));
        app.busy = false;
        app.current_job = None;
        app.handle(Action::Login {
            server: "https://cloud.example".into(),
            email: "a@example.test".into(),
            password: String::new(),
            second_factor: Some("abcde-fg2h3".into()),
        });
        assert!(matches!(
            requests.try_recv().map(|job| job.request),
            Ok(Request::SecondFactor {
                proof: Proof::Recovery(_),
                ..
            })
        ));
        // Garbage is not sent at all.
        app.busy = false;
        app.current_job = None;
        app.handle(Action::Login {
            server: "https://cloud.example".into(),
            email: "a@example.test".into(),
            password: String::new(),
            second_factor: Some("12".into()),
        });
        assert!(requests.try_recv().is_err());
        // Without an open challenge the password login is used.
        app.picker.second_factor_challenge_id = None;
        app.handle(Action::Login {
            server: "https://cloud.example".into(),
            email: "a@example.test".into(),
            password: "pw".into(),
            second_factor: None,
        });
        assert!(matches!(
            requests.try_recv().map(|job| job.request),
            Ok(Request::Login { .. })
        ));
    }

    #[test]
    fn rejected_second_factor_keeps_the_code_dialog_and_signing_in_clears_it() {
        let (mut app, _requests, event_sender) = test_app();
        app.picker.requires_second_factor = true;
        app.picker.second_factor_challenge_id = Some("challenge-1".into());
        app.current_job = Some((40, Arc::new(AtomicBool::new(false))));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(40),
                event: Event::SecondFactorRejected("rejected".into()),
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert_eq!(app.picker.dialog, Some(Dialog::Login));
        assert!(app.picker.requires_second_factor);
        assert_eq!(app.picker.error.as_deref(), Some("rejected"));

        app.current_job = Some((41, Arc::new(AtomicBool::new(false))));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(41),
                event: Event::LoggedIn {
                    display_name: "A".into(),
                    email: "a@example.test".into(),
                },
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert!(!app.picker.requires_second_factor);
        assert_eq!(app.picker.second_factor_challenge_id, None);
    }

    #[test]
    fn a_logout_the_server_did_not_confirm_is_shown_to_the_user() {
        let (mut app, _requests, event_sender) = test_app();
        app.current_job = Some((50, Arc::new(AtomicBool::new(false))));
        app.busy = true;
        event_sender
            .send(WorkerEvent {
                id: Some(50),
                event: Event::LoggedOutLocally("CraftCloud is unreachable".into()),
            })
            .ok();
        app.process_events(&egui::Context::default());
        assert_eq!(app.picker.dialog, Some(Dialog::Login));
        assert!(
            app.picker
                .error
                .as_deref()
                .is_some_and(|text| text.contains("Signed out locally")
                    && text.contains("could not be ended")),
            "{:?}",
            app.picker.error
        );
    }

    #[test]
    fn data_dir_is_per_user_and_never_a_shared_temp_fallback() {
        let env = |pairs: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            move |name: &str| {
                owned
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| std::ffi::OsString::from(value))
            }
        };
        let home = if cfg!(windows) {
            "C:\\Users\\a"
        } else {
            "/home/a"
        };
        let local = if cfg!(windows) {
            "C:\\Users\\a\\AppData\\Local"
        } else {
            "/local"
        };
        let data = if cfg!(windows) {
            "C:\\private"
        } else {
            "/private"
        };
        assert!(
            resolve_data_dir(Platform::Windows, env(&[("LOCALAPPDATA", local)]))
                .is_ok_and(|dir| dir.ends_with("CraftCloudDemo") && dir.starts_with(local))
        );
        assert!(
            resolve_data_dir(Platform::Unix, env(&[("HOME", home)]))
                .is_ok_and(|dir| dir.starts_with(home) && dir.ends_with("CraftCloudDemo"))
        );
        assert!(
            resolve_data_dir(Platform::MacOs, env(&[("HOME", home)]))
                .is_ok_and(|dir| dir.to_string_lossy().contains("Application Support"))
        );
        assert_eq!(
            resolve_data_dir(Platform::Unix, env(&[("CRAFTCLOUD_DEMO_DATA_DIR", data)])).ok(),
            Some(PathBuf::from(data))
        );
        // Nothing usable: an error, not the temp directory. Relative values do not count.
        for platform in [Platform::Windows, Platform::MacOs, Platform::Unix] {
            let result = resolve_data_dir(platform, env(&[("HOME", "relative"), ("APPDATA", "x")]));
            assert!(
                result.is_err_and(|message| message.contains("CRAFTCLOUD_DEMO_DATA_DIR")),
                "relative or missing variables must fail"
            );
        }
    }
}
