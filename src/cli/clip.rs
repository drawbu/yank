//! The clipboard commands.

use std::{
    fs,
    io::{IsTerminal as _, Read as _, Write as _},
    path::PathBuf,
    time::Duration,
};

use clap::Args;
use eyre::WrapErr as _;

use super::{
    Situation,
    client::{request, unexpected},
    parse_duration, ui,
};
use crate::{
    clip::mime::PLAIN_TEXT,
    config::Dirs,
    daemon::control::{Request, Response},
};

/// Copy something to every machine
///
/// Reads standard input when no text is given, so it works in a pipe the
/// way `wl-copy` does.
#[derive(Debug, Args)]
pub struct CopyArgs {
    /// Text to copy; standard input is read when this is left out
    #[arg(trailing_var_arg = true)]
    text: Vec<String>,

    /// Drop it everywhere after this long, for example `90s` or `5m`
    ///
    /// Every machine removes it on its own when the time is up, network or
    /// no network, and empties the clipboard if it is still what it holds.
    #[arg(long, short = 't', value_name = "DURATION", value_parser = parse_duration)]
    ttl: Option<Duration>,

    /// Treat it as a password
    ///
    /// It is shared, but never written to disk on any machine, never shown,
    /// and given a lifetime (`secret-ttl` in config.toml) unless `--ttl`
    /// says otherwise.
    #[arg(long, short = 's')]
    secret: bool,

    /// Type to copy it as
    #[arg(long, value_name = "MIME", default_value = PLAIN_TEXT)]
    mime: String,

    /// File or folder to copy, contents and all; repeatable
    ///
    /// The other machines get the file itself, not the path, and pasting
    /// it there writes it wherever it is pasted.
    ///
    /// Not for a secret: sharing a file writes it to disk on every machine,
    /// which is the one thing `--secret` promises never happens.
    #[arg(
        long = "file",
        short = 'f',
        value_name = "PATH",
        conflicts_with_all = ["mime", "secret"],
    )]
    files: Vec<PathBuf>,
}

/// Write the files on the clipboard here
///
/// The daemon fetches them from whoever has them, which takes as long as
/// the files are large.
#[derive(Debug, Args)]
pub struct GetArgs {
    /// Where to write them
    #[arg(long, short = 'C', value_name = "DIR", default_value = ".")]
    to: PathBuf,
}

/// Print what is on the clipboard
#[derive(Debug, Args)]
pub struct PasteArgs {
    /// Print the types the clipboard carries instead of the contents
    #[arg(long)]
    mime: bool,

    /// Type to print it in, when the clipboard carries several
    #[arg(long = "type", value_name = "MIME")]
    mime_type: Option<String>,
}

pub fn copy(args: CopyArgs, dirs: &Dirs) -> eyre::Result<()> {
    let copy = if args.files.is_empty() {
        let bytes = if args.text.is_empty() {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes)?;
            bytes
        } else {
            args.text.join(" ").into_bytes()
        };
        Request::Copy {
            mime: args.mime,
            bytes,
            secret: args.secret,
            ttl: args.ttl,
        }
    } else {
        // Files themselves rather than the text of their paths.
        eyre::ensure!(args.text.is_empty(), "copy either text or files, not both");
        let paths = args
            .files
            .iter()
            .map(|path| {
                // The other machines are told where this came from as well
                // as what it is, and a relative path means nothing there.
                let path = path.canonicalize()?;
                Ok(path.to_string_lossy().into_owned())
            })
            .collect::<std::io::Result<Vec<String>>>()?;
        Request::CopyFiles {
            paths,
            ttl: args.ttl,
        }
    };

    match request(dirs, &copy)? {
        Response::Done => {
            anstream::println!("{}", ui::good("Copied"));
            Ok(())
        }
        other => Err(unexpected(&other)),
    }
}

/// Writes the clipboard's files here.
///
/// The daemon answers once they are laid out on this machine, so what
/// takes time is the transfer itself.
pub fn get(args: &GetArgs, dirs: &Dirs) -> eyre::Result<()> {
    let response = request(dirs, &Request::Files)?;
    let Response::Files { tree, files } = response else {
        return Err(unexpected(&response));
    };
    let Some(tree) = tree else {
        return Err(Situation::FilesPending.into());
    };
    let tree = PathBuf::from(tree);

    // Checked before anything is written, so a collision halfway through
    // does not leave half a copy behind.
    for file in &files {
        let to = args.to.join(&file.path);
        eyre::ensure!(!to.exists(), "{} is already there", to.display());
    }
    for file in &files {
        let (from, to) = (tree.join(&file.path), args.to.join(&file.path));
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        // Sharing the blocks with the spool where the filesystem can, the
        // way spooling them did: getting a four gigabyte image out costs a
        // directory entry rather than four gigabytes.
        fs::copy(&from, &to).wrap_err_with(|| format!("cannot write {}", to.display()))?;
    }

    anstream::println!(
        "{}",
        ui::good(format_args!(
            "Wrote {} file(s) to {}",
            files.len(),
            args.to.display(),
        )),
    );

    Ok(())
}

pub fn paste(args: &PasteArgs, dirs: &Dirs) -> eyre::Result<()> {
    let response = request(
        dirs,
        &Request::Paste {
            mime: args.mime_type.clone(),
        },
    )?;

    match response {
        Response::Contents {
            mime,
            alternates,
            bytes,
        } => {
            if args.mime {
                for mime in std::iter::once(mime).chain(alternates) {
                    anstream::println!("{mime}");
                }
                return Ok(());
            }

            let mut out = std::io::stdout().lock();
            out.write_all(&bytes)?;
            // A terminal wants the prompt back on its own line; a pipe
            // wants exactly the bytes that were copied.
            if out.is_terminal() && !bytes.ends_with(b"\n") {
                out.write_all(b"\n")?;
            }
            out.flush()?;

            Ok(())
        }
        other => Err(unexpected(&other)),
    }
}

pub fn clear(dirs: &Dirs) -> eyre::Result<()> {
    match request(dirs, &Request::Clear)? {
        Response::Done => {
            anstream::println!("{}", ui::good("Cleared the clipboard everywhere"));
            Ok(())
        }
        other => Err(unexpected(&other)),
    }
}
