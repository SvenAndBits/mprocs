use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum DkRequest {
  Spawn {
    path: String,
    cmd: Vec<String>,
    cwd: Option<String>,
  },
  Ls {
    glob: Option<String>,
  },
  Start {
    path: String,
  },
  Stop {
    path: String,
  },
  Kill {
    path: String,
  },
  Restart {
    path: String,
  },
  Screen {
    path: String,
  },
  Inspect {
    path: String,
  },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum DkResponse {
  Ok,
  TaskList(Vec<DkTaskInfo>),
  Screen(Option<String>),
  TaskDetail(Option<DkTaskDetail>),
  Error(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DkTaskInfo {
  pub path: String,
  pub status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DkTaskDetail {
  pub path: String,
  pub status: String,
  pub deps: Vec<DkTaskInfo>,
  pub children: Vec<DkTaskInfo>,
}
