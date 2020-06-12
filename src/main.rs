#[macro_use]
extern crate cfg_if;

use std::env;
use std::fs::{read_dir, Metadata, create_dir, remove_dir_all};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::str::{self, FromStr};
use std::thread;

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
#[allow(dead_code)]
enum ResultCode {
    RestartMarkerReply = 110,
    ServiceReadInXXXMinutes = 120,
    DataConnectionAlreadyOpen = 125,
    FileStatusOk = 150,
    Ok = 200,
    CommandNotImplementedSuperfluousAtThisSite = 202,
    SystemStatus = 211,
    DirectoryStatus = 212,
    FileStatus = 213,
    HelpMessage = 214,
    SystemType = 215,
    ServiceReadyForNewUser = 220,
    ServiceClosingControlConnection = 221,
    DataConnectionOpen = 225,
    ClosingDataConnection = 226,
    EnteringPassiveMode = 227,
    UserLoggedIn = 230,
    RequestedFileActionOkay = 250,
    PATHNAMECreated = 257,
    UserNameOkayNeedPassword = 331,
    NeedAccountForLogin = 332,
    RequestedFileActionPendingFurtherInformation = 350,
    ServiceNotAvailable = 421,
    CantOpenDataConnection = 425,
    ConnectionClosed = 426,
    FileBusy = 450,
    LocalErrorInProcessing = 451,
    InsufficientStorageSpace = 452,
    UnknownCommand = 500,
    InvalidParameterOrArgument = 501,
    CommandNotImplemented = 502,
    BadSequenceOfCommands = 503,
    CommandNotImplementedForThatParameter = 504,
    NotLoggedIn = 530,
    NeedAccountForStoringFiles = 532,
    FileNotFound = 550,
    PageTypeUnknown = 551,
    ExceededStorageAllocation = 552,
    FileNameNotAllowed = 553,
}

#[derive(Clone, Debug)]
enum Command {
    Auth,
    Syst,
    User(String),
    Noop,
    Pwd,
    Type,
    Pasv,
    List(PathBuf),
    Cwd(PathBuf),
    CdUp,
    Mkd(PathBuf),
    Rmd(PathBuf),
    Unknown(String),
}

impl AsRef<str> for Command {
    fn as_ref(&self) -> &str {
        match *self {
            Command::Auth => "AUTH",
            Command::Syst => "SYST",
            Command::User(_) => "USER",
            Command::Noop => "NOOP",
            Command::Pwd => "PWD",
            Command::Type => "TYPE",
            Command::Pasv => "PASV",
            Command::List(_) => "LIST",
            Command::Cwd(_) => "CWD",
            Command::CdUp => "CDUP",
            Command::Mkd(_) => "MKD",
            Command::Rmd(_) => "RMD",
            Command::Unknown(_) => "UNKN",
        }
    }
}

impl Command {
    pub fn new(input: Vec<u8>) -> io::Result<Self> {
        let mut iter = input.split(|&byte| byte == b' ');
        let mut command = iter.next().expect("command in input").to_vec();
        to_uppercase(&mut command);
        let data = iter.next();
        let command = match command.as_slice() {
            b"AUTH" => Command::Auth,
            b"SYST" => Command::Syst,
            b"USER" => Command::User(
                data.map(|bytes| {
                    String::from_utf8(bytes.to_vec()).expect("Cannot convert bytes to String")
                })
                .unwrap_or_default(),
            ),
            b"NOOP" => Command::Noop,
            b"PWD" => Command::Pwd,
            b"TYPE" => Command::Type,
            b"PASV" => Command::Pasv,
            b"LIST" => Command::List(
                if let Some(path) = data {
                    Path::new(str::from_utf8(path).unwrap()).to_path_buf()
                } else {
                    PathBuf::from_str(".").unwrap()
                }
            ),
            b"CWD" => Command::Cwd(
                data.map(|bytes| Path::new(str::from_utf8(bytes).unwrap()).to_path_buf())
                    .unwrap(),
            ),
            b"CDUP" => Command::CdUp,
            b"MKD" => Command::Mkd(data.map(|bytes| Path::new(str::from_utf8(bytes).unwrap()).to_path_buf())
            .unwrap()),
            b"RMD" => Command::Rmd(data.map(|bytes| Path::new(str::from_utf8(bytes).unwrap()).to_path_buf())
            .unwrap()),
            s => Command::Unknown(str::from_utf8(s).unwrap_or("").to_owned()),
        };
        Ok(command)
    }
}

cfg_if! {
    if #[cfg(windows)] {
        fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
            use std::os::windows::prelude::*;
            (time::at(time::Timespec::new(meta.last_write_time())), meta.file_size())
        }
    } else {
        fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
            use std::os::unix::prelude::*;
            (time::at(time::Timespec::new(meta.mtime(), 0)), meta.size())
        }
    }
}

#[allow(dead_code)]
struct Client {
    cwd: PathBuf,
    stream: TcpStream,
    name: Option<String>,
    data_writer: Option<TcpStream>,
}

impl Client {
    fn new(stream: TcpStream) -> Client {
        Client {
            cwd: PathBuf::from("/"),
            stream: stream,
            name: None,
            data_writer: None,
        }
    }

    fn complete_path(&self, path: PathBuf, server_root: &PathBuf) -> Result<PathBuf, io::Error> {
        let directory = server_root.join(if path.has_root() {
            path.iter().skip(1).collect()
        } else {
            path
        });

        let dir = directory.canonicalize();
        if let Ok(ref dir) = dir {
            if !dir.starts_with(&server_root) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        dir
    }

    fn cwd(&mut self, directory: PathBuf) {
        let server_root = env::current_dir().unwrap();
        let path = self.cwd.join(&directory);
        if let Ok(dir) = self.complete_path(path, &server_root) {
            if let Ok(prefix) = dir.strip_prefix(&server_root).map(|p| p.to_path_buf()) {
                if prefix.to_str().unwrap().is_empty() {
                    self.cwd = PathBuf::from_str("/").unwrap();
                } else {
                    self.cwd = prefix
                }
                println!("current cwd: {}", self.cwd.to_str().unwrap());
                send_cmd(
                    &mut self.stream,
                    ResultCode::Ok,
                    &format!("Directory changed to \"{}\"", directory.display()),
                );
                return;
            }
        }
        send_cmd(
            &mut self.stream,
            ResultCode::FileNotFound,
            "No such file or directory",
        );
    }

    fn mkd(&mut self, path: PathBuf) {
        let server_root = env::current_dir().unwrap();
        let path = self.cwd.join(&path);
        if let Some(parent) = path.parent().map(|p| p.to_path_buf()) {
            if let Ok(mut dir) = self.complete_path(parent, &server_root) {
                if dir.is_dir() {
                    if let Some(filename) = path.file_name().map(|p| p.to_os_string()) {
                        dir.push(filename);
                        if create_dir(dir).is_ok() {
                            send_cmd(&mut self.stream, ResultCode::PATHNAMECreated, "Folder successfully created!");
                            return
                        }
                    }
                }
            }
        }
        send_cmd(&mut self.stream, ResultCode::FileNotFound, "Cound't create folder");
    }

    fn rmd(&mut self, path: PathBuf) {
        let server_root = env::current_dir().unwrap();
        if let Ok(path) = self.complete_path(path, &server_root) {
            if remove_dir_all(path).is_ok() {
                send_cmd(&mut self.stream, ResultCode::RequestedFileActionOkay, "Folder successfully removed!");
                return
            }
        }
        send_cmd(&mut self.stream, ResultCode::FileNotFound, "Coundn't remove folder!");
    }

    fn handle_cmd(&mut self, cmd: Command) {
        println!("====> {:?}", cmd);
        match cmd {
            Command::Auth => send_cmd(
                &mut self.stream,
                ResultCode::CommandNotImplemented,
                "Not implemented",
            ),
            Command::Syst => send_cmd(&mut self.stream, ResultCode::Ok, "I won't tell"),
            Command::User(username) => {
                if username.is_empty() {
                    send_cmd(
                        &mut self.stream,
                        ResultCode::InvalidParameterOrArgument,
                        "Invalid username",
                    )
                } else {
                    self.name = Some(username.to_owned());
                    send_cmd(
                        &mut self.stream,
                        ResultCode::UserLoggedIn,
                        &format!("Welcome {}!", username),
                    );
                }
            }
            Command::Noop => send_cmd(&mut self.stream, ResultCode::Ok, "Doing nothing..."),
            Command::Pwd => {
                let msg = format!("{}", self.cwd.to_str().unwrap_or(""));
                if !msg.is_empty() {
                    let message = format!("\"{}\" ", msg);
                    send_cmd(
                        &mut self.stream,
                        ResultCode::PATHNAMECreated,
                        message.as_str(),
                    )
                } else {
                    send_cmd(
                        &mut self.stream,
                        ResultCode::FileNotFound,
                        "No such file or directory",
                    )
                }
            }
            Command::Type => send_cmd(
                &mut self.stream,
                ResultCode::Ok,
                "Transfer type changed successfully",
            ),
            Command::Pasv => {
                if self.data_writer.is_some() {
                    send_cmd(
                        &mut self.stream,
                        ResultCode::DataConnectionAlreadyOpen,
                        "Already listen...",
                    )
                } else {
                    let port = 43210;
                    send_cmd(
                        &mut self.stream,
                        ResultCode::EnteringPassiveMode,
                        &format!("127,0,0,1, {}, {}", port >> 8, port & 0xff),
                    );
                    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
                    let listener = TcpListener::bind(&addr).unwrap();
                    match listener.incoming().next() {
                        Some(Ok(client)) => {
                            self.data_writer = Some(client);
                        }
                        _ => {
                            send_cmd(
                                &mut self.stream,
                                ResultCode::ServiceNotAvailable,
                                "issues happen...",
                            );
                        }
                    }
                }
            },
            Command::List(path) => {

                let server_root = env::current_dir().unwrap();
                let path = self.cwd.join(&path);
                let real_path = self.complete_path(path, &server_root);

                if let Some(ref mut data_writer) = self.data_writer {

                    if let Ok(path) = real_path {
                        send_cmd(
                            &mut self.stream,
                            ResultCode::DataConnectionAlreadyOpen,
                            "Starting to list directory...",
                        );

                        let mut out = String::new();
                        if path.is_dir() {
                            for entry in read_dir(path).unwrap() {
                                if let Ok(entry) = entry {
                                    add_file_info(entry.path(), &mut out);
                                }
                                send_data(data_writer, &out)
                            }
                        } else {
                            add_file_info(path, &mut out);
                        }
                    } else {
                        send_cmd(
                            &mut self.stream,
                            ResultCode::DataConnectionAlreadyOpen,
                            "No such file or directory...",
                        );
                    }
                } else {
                    send_cmd(
                        &mut self.stream,
                        ResultCode::ConnectionClosed,
                        "No opened data connection",
                    );
                }

                if self.data_writer.is_some() {
                    self.data_writer = None;
                    send_cmd(
                        &mut self.stream,
                        ResultCode::ClosingDataConnection,
                        "Transfer done",
                    );
                }
            },
            Command::Cwd(directory) => self.cwd(directory),
            Command::CdUp => {
                if let Some(path) = self.cwd.parent().map(Path::to_path_buf) {
                    self.cwd = path;
                }
                send_cmd(&mut self.stream, ResultCode::Ok, "Done");
            },
            Command::Mkd(path) => {
                self.mkd(path);
            },
            Command::Rmd(path) => {
                self.rmd(path);
            },
            Command::Unknown(_s) => send_cmd(
                &mut self.stream,
                ResultCode::CommandNotImplemented,
                "Not implemented",
            ),
        }
    }
}

fn to_uppercase(data: &mut [u8]) {
    for byte in data {
        if *byte >= 'a' as u8 && *byte <= 'z' as u8 {
            *byte -= 32;
        }
    }
}

fn send_cmd(stream: &mut TcpStream, code: ResultCode, message: &str) {
    let msg = if message.is_empty() {
        format!("{}\r\n", code as u32)
    } else {
        format!("{} {}\r\n", code as u32, message)
    };

    println!("<==== {}", msg);
    write!(stream, "{}", msg).unwrap()
}

fn read_all_message(stream: &mut TcpStream) -> Vec<u8> {
    let buf = &mut [0; 1];
    let mut out = Vec::with_capacity(100);

    loop {
        match stream.read(buf) {
            Ok(received) if received > 0 => {
                if out.is_empty() && buf[0] == b' ' {
                    continue;
                }
                out.push(buf[0]);
            }
            _ => return Vec::new(),
        }
        let len = out.len();
        if len > 1 && out[len - 2] == b'\r' && out[len - 1] == b'\n' {
            out.pop();
            out.pop();
            return out;
        }
    }
}

fn handle_client(mut stream: TcpStream) {
    println!("new client connected!");
    send_cmd(
        &mut stream,
        ResultCode::ServiceReadyForNewUser,
        "Welcome to this FTP server!",
    );
    let mut client = Client::new(stream);
    loop {
        let data = read_all_message(&mut client.stream);
        if data.is_empty() {
            println!("client disconnected...");
            break;
        }
        client.handle_cmd(Command::new(data).unwrap());
    }
}

fn send_data(stream: &mut TcpStream, s: &str) {
    write!(stream, "{}", s).unwrap();
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sept", "Oct", "Nov", "Dec",
];
fn add_file_info(path: PathBuf, out: &mut String) {
    let extra = if path.is_dir() { "/" } else { "" };
    let is_dir = if path.is_dir() { "d" } else { "-" };

    let meta = match ::std::fs::metadata(&path) {
        Ok(meta) => meta,
        _ => return,
    };

    let (time, file_size) = get_file_info(&meta);
    let path = match path.to_str() {
        Some(path) => match path.split("/").last() {
            Some(path) => path,
            _ => return,
        },
        _ => return,
    };

    let rights = if meta.permissions().readonly() {
        "r--r--r--"
    } else {
        "rw-rw-rw-"
    };
    let file_str = format!("{is_dir}{rights} {links} {owner} {group} {size} {month} {day} {hour}:{min} {path}{extra}\r\n",
                                    is_dir=is_dir,
                                    rights=rights,
                                    links=1,
                                    owner="anonymous",
                                    group="anonymous",
                                    size=file_size,
                                    month=MONTHS[time.tm_mon as usize],
                                    day=time.tm_mday,
                                    hour=time.tm_hour,
                                    min=time.tm_min,
                                    path=path,
                                    extra=extra
                                    );
    out.push_str(&file_str);
    println!("==> {:?}", &file_str);
}

fn main() {
    let listener = TcpListener::bind("0.0.0.0:1234").expect("Coundn't bind this address...");
    println!("Waiting for clients to connect...");

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            thread::spawn(move || handle_client(stream));
        } else {
            println!("A client tried to connect...")
        }
    }
}
