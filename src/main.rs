#![allow(dead_code)]

use std::path::{Path, PathBuf};
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use tokio::fs::File;
use std::pin::Pin;
use std::future::Future;
use std::process::{Stdio, ExitStatus};
use std::os::unix::io::{RawFd, AsRawFd, FromRawFd, IntoRawFd};
use std::ffi::OsStr;
use tokio::process::Command;
use tokio::task::JoinError;
use tokio::join;

type RunValue<T> =
    Pin<Box<dyn Future<Output=Result<T>> + Send>>;

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
enum Error {
    Io(std::io::Error),
    Join(JoinError),
    Nix(nix::Error),
}

impl From<tokio::task::JoinError> for Error {
    fn from(v : tokio::task::JoinError) -> Self {
        Error::Join(v)
    }
}

impl From<std::io::Error> for Error {
    fn from(v : std::io::Error) -> Self {
        Error::Io(v)
    }
}

impl From<nix::Error> for Error {
    fn from(v : nix::Error) -> Self {
        Error::Nix(v)
    }
}

trait Status {
    fn success(&self) -> bool;
}

impl Status for ExitStatus {
    fn success(&self) -> bool {
        ExitStatus::success(self)
    }
}

fn close_on_exec<T : AsRawFd>(fd : T) -> Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag};

    fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    Ok(())
}

trait Cmd {
    type Status : Status + Send + 'static;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()>;
    fn stderr<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> ;
    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()>;

    fn run(self) -> RunValue<Self::Status>;

    fn pipe_fn<F>(self, f : F) -> PipeFn<Self, F>
    where
        F : FnOnce(Vec<u8>) -> Result<Vec<u8>>,
        Self : Sized,
    {
        PipeFn::new(self, f, false)
    }

    fn pipe_all_fn<F>(self, f : F) -> PipeFn<Self, F>
    where
        F : FnOnce(Vec<u8>) -> Result<Vec<u8>>,
        Self : Sized,
    {
        PipeFn::new(self, f, true)
    }

    /// self | other
    fn pipe<U>(mut self, mut other : U) -> Result<Pipe<Self, U>>
    where
        U : Cmd,
        Self : Sized
    {

        let (recv, send) = nix::unistd::pipe()?;

        close_on_exec(send)?;
        close_on_exec(recv)?;

        self.stdout(send.into_raw_fd())?;
        other.stdin(recv.into_raw_fd())?;

        Ok(Pipe(self, other))
    }

    /// self 2>&1 | other
    fn pipe_all<U>(mut self, mut other : U) -> Result<Pipe<Self, U>>
    where
        U : Cmd,
        Self : Sized
    {
        let (recv, send) = nix::unistd::pipe()?;

        let send_fd0 = send.into_raw_fd();
        let send_fd1 = nix::unistd::dup(send_fd0)?;

        close_on_exec(send_fd0)?;
        close_on_exec(send_fd1)?;
        close_on_exec(recv)?;

        self.stdout(send_fd0)?;
        self.stderr(send_fd1)?;

        other.stdin(recv)?;

        Ok(Pipe(self, other))
    }

    /// self && other
    fn and<U>(self, other : U) -> Result<And<Self, U>>
    where
        U : Cmd,
        Self : Sized
    {
        Ok(And(self, other))
    }

    /// self || other
    fn or<U>(self, other : U) -> Result<Or<Self, U>>
    where
        U : Cmd,
        Self : Sized
    {
        Ok(Or(self, other))
    }
}

struct PipeFn<C, F>{
    f : F,
    prev : C,
    out : Option<RawFd>,
    with_err : bool
}

impl<C, F> PipeFn<C, F> {
    fn new(prev : C, f : F, with_err : bool) -> Self {
        PipeFn{
            out : None,
            f,
            prev,
            with_err,
        }
    }
}

struct PipeFnStatus<A>(A, Option<Result<()>>);

impl<A> Status for PipeFnStatus<A>
where
    A : Status
{
    fn success(&self) -> bool {
        self.0.success() && self.1.as_ref().map_or(false, Result::is_ok)
    }
}

impl<C, F> Cmd for PipeFn<C, F>
where
    C : Cmd + Send + 'static,
    F : FnOnce(Vec<u8>) -> Result<Vec<u8>> + Send + 'static
{
    type Status = PipeFnStatus<C::Status>;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.out = Some(cfg.into_raw_fd());
        Ok(())
    }

    fn stderr<T : IntoRawFd>(&mut self, _cfg : T) -> Result<()> {
        Ok(())
    }

    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.prev.stdin(cfg)?;
        Ok(())
    }

    fn run(mut self) -> RunValue<Self::Status> {
        Box::pin(async move {

            let (recv, send) = nix::unistd::pipe()?;

            if self.with_err {
                let send1 = nix::unistd::dup(send)?;
                self.prev.stderr(send1)?;
            }

            self.prev.stdout(send)?;

            let mut recv_async = unsafe {
                File::from_raw_fd(recv.into_raw_fd())
            };

            let prev = self.prev;

            let status_fut = tokio::spawn(async {
                prev.run().await
            });

            let mut buf = Vec::new();
            recv_async.read_to_end(&mut buf).await?;

            let status = status_fut.await??;
            if !status.success() {
                return Ok(PipeFnStatus(status, None))
            }

            let buf = match (self.f)(buf) {
                Ok(buf) => buf,
                Err(err) => {
                    return Ok(PipeFnStatus(status, Some(Err(err))))
                }
            };

            match self.out {
                Some(fd) => {
                    let mut out = unsafe {
                        File::from_raw_fd(fd)
                    };

                    Pin::new(&mut out).write_all(&buf).await?;
                    out.flush().await?;

                    Ok(PipeFnStatus(status, Some(Ok(()))))
                },
                None => {
                    let mut out = tokio::io::stdout();
                    Pin::new(&mut out).write_all(&buf).await?;
                    out.flush().await?;

                    Ok(PipeFnStatus(status, Some(Ok(()))))
                }
            }
        })
    }
}

#[derive(Debug)]
struct Pipe<A, B>(A, B);

#[derive(Debug)]
struct PipeStatus<A, B>(A, B);

impl<A, B> Status for PipeStatus<A, B>
where
    A : Status,
    B : Status
{
    fn success(&self) -> bool {
        self.0.success() && self.1.success()
    }
}

impl<A, B> Cmd for Pipe<A, B>
where
    A : Cmd + Send + 'static,
    B : Cmd + Send + 'static,
{
    type Status = PipeStatus<A::Status, B::Status>;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.1.stdout(cfg)?;
        Ok(())
    }

    fn stderr<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.1.stderr(cfg)?;
        Ok(())
    }

    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.0.stdin(cfg)?;
        Ok(())
    }

    fn run(self) -> RunValue<Self::Status> {
        Box::pin(async move {
            let a = self.0.run();
            let b = self.1.run();

            let (aa, bb) = join!(a, b);

            Ok(PipeStatus(aa?, bb?))
        })
    }
}

#[derive(Debug)]
struct Or<A, B>(A, B);

#[derive(Debug)]
struct OrStatus<A, B>(A, Option<B>);

impl<A, B> Status for OrStatus<A, B>
where
    A : Status,
    B : Status
{
    fn success(&self) -> bool {
        self.0.success() || self.1.as_ref().map_or(false, B::success)
    }
}

impl<A, B> Cmd for Or<A, B>
where
    A : Cmd + Send + 'static,
    B : Cmd + Send + 'static,
{

    type Status = OrStatus<A::Status, B::Status>;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let fd = cfg.into_raw_fd();
        self.0.stdout(nix::unistd::dup(fd)?)?;
        self.1.stdout(fd)
    }

    fn stderr<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let fd = cfg.into_raw_fd();
        self.0.stderr(nix::unistd::dup(fd)?)?;
        self.1.stderr(fd)
    }

    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let fd = cfg.into_raw_fd();
        self.0.stdin(nix::unistd::dup(fd)?)?;
        self.1.stdin(fd)
    }

    fn run(self) -> RunValue<Self::Status> {
        Box::pin(async move {
            let a = self.0.run().await?;
            if a.success() {
                return Ok(OrStatus(a, None))
            }

            let b = self.1.run().await?;

            Ok(OrStatus(a, Some(b)))
        })
    }
}

#[derive(Debug)]
struct And<A, B>(A, B);

#[derive(Debug)]
struct AndStatus<A, B>(A, Option<B>);
impl<A, B> Status for AndStatus<A, B>
where
    A : Status,
    B : Status
{
    fn success(&self) -> bool {
        self.0.success() && self.1.as_ref().map_or(false, B::success)
    }
}

impl<A, B> Cmd for And<A, B>
where
    A : Cmd + Send + 'static,
    B : Cmd + Send + 'static,
{

    type Status = AndStatus<A::Status, B::Status>;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let fd = cfg.into_raw_fd();
        self.0.stdout(nix::unistd::dup(fd)?)?;
        self.1.stdout(fd)
    }

    fn stderr<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let fd = cfg.into_raw_fd();
        self.0.stderr(nix::unistd::dup(fd)?)?;
        self.1.stderr(fd)
    }

    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let fd = cfg.into_raw_fd();
        self.0.stdin(nix::unistd::dup(fd)?)?;
        self.1.stdin(fd)
    }

    fn run(self) -> RunValue<Self::Status> {
        Box::pin(async move {
            let a = self.0.run().await?;

            if !a.success() {
                return Ok(AndStatus(a, None))
            }

            let b = self.1.run().await?;

            Ok(AndStatus(a, Some(b)))
        })
    }
}

struct Plain(Command);

impl Cmd for Plain {
    type Status = ExitStatus;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let x = unsafe {
            Stdio::from_raw_fd(cfg.into_raw_fd())
        };

        self.0.stdout(x);
        Ok(())
    }

    fn stderr<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let x = unsafe {
            Stdio::from_raw_fd(cfg.into_raw_fd())
        };

        self.0.stderr(x);
        Ok(())
    }

    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        let x = unsafe {
            Stdio::from_raw_fd(cfg.into_raw_fd())
        };

        self.0.stdin(x);
        Ok(())
    }

    fn run(mut self) -> RunValue<Self::Status> {
        Box::pin(async move {
            let ret = self.0.status().await?;
            Ok(ret)
        })
    }
}

fn cmd<S : AsRef<OsStr>>(name : &S, args : &[S]) -> Plain {
    let mut c = Command::new(name);
    c.args(args);

    Plain(c)
}

/// calls set_current_dir
fn cd<P : AsRef<std::path::Path>>(path : P) -> Result<()> {
    std::env::set_current_dir(path)?;
    Ok(())
}

fn wd() -> Result<PathBuf> {
    Ok(std::env::current_dir()?)
}

struct WithWd<P, C>(P, C);

fn with_wd<P, C>(p : P, c : C) -> WithWd<P, C> {
    WithWd(p, c)
}

impl<P, C> Cmd for WithWd<P, C>
where
    P : AsRef<Path> + Send + 'static,
    C : Cmd + Send + 'static
{
    type Status = C::Status;

    fn stdout<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.1.stdout(cfg)
    }

    fn stderr<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.1.stderr(cfg)
    }

    fn stdin<T : IntoRawFd>(&mut self, cfg : T) -> Result<()> {
        self.1.stdin(cfg)
    }

    fn run(self) -> RunValue<Self::Status> {
        Box::pin(async move {
            let cur = wd()?;
            cd(self.0)?;
            let s = self.1.run().await?;
            cd(cur)?;
            Ok(s)
        })
    }
}


#[tokio::main]
async fn main() {
    cmd(&"./echo_err", &["a", "b"])
        .pipe_all_fn(|mut x| {

            x.iter_mut().for_each(|c| {
                if ('a'..'z').contains(&(*c as char)) {
                    *c = *c - ('a' as u8 - ('A' as u8))
                }
            });

            Ok(x)
        })
        .run().await
        .unwrap();

    cmd(&"./echo_err", &["a", "b"])
        .pipe_all(cmd(&"grep", &["a"]))
        .unwrap()
        .run().await
        .unwrap();
    /*
    cmd(&"ls", &["--help"])
        .pipe_all_fn(|x| {
            println!("{}", x[0]);
            Ok(x)
        })
        .run().await
        .unwrap();

    with_wd("/",
        cmd(&"ls", &[])
            .and(cmd(&"pwd", &[]))
            .unwrap()
            .pipe_fn(|buf| {
                println!("{}", buf[0]);
                Ok(vec![65, 66, 67, 10])
            })
        )
        .run().await
        .unwrap();
    cmd(&"git", &[&"status"])
        .pipe(cmd(&"grep", &[&"file"]))
        .unwrap()
        .pipe(cmd(&"grep", &[&"nothing"]))
        .unwrap()
        .run().await
        .unwrap();
        */

    println!("done!")
}
