use rand::distributions::DistString;
use ssh2::Session;
use std::io::{Read, Write};
use std::net::TcpStream;

pub struct RemoteStartOptions {
    pub address: String,
    pub ssh_port: u16,
    pub username: String,
    pub identity_file: Option<String>,
    pub workers: u8,
    pub server_port: u16,
}

fn try_ssh_auth_with_agent(session: &Session, username: &str) -> Result<(), anyhow::Error> {
    let mut agent = session.agent()?;
    agent.connect()?;
    agent.list_identities()?;
    for identity in agent.identities()? {
        match agent.userauth(username, &identity) {
            Ok(_) => break,
            Err(_) => continue,
        }
    }
    Ok(())
}

/// Upload deltasync program to remote and and start it in server mode with a random secret.
pub fn remote_start_server(options: RemoteStartOptions) -> Result<String, anyhow::Error> {
    let address = options.address + ":" + &options.ssh_port.to_string();

    let tcp = TcpStream::connect(&address)?;

    let mut session = Session::new()?;
    session.set_tcp_stream(tcp);
    session.handshake()?;

    println!("Connecting to {}@{}...", options.username, address);

    if let Some(identity_file) = options.identity_file {
        let identity_file = std::path::Path::new(&identity_file);
        let _ = session.userauth_pubkey_file(&options.username, None, identity_file, None);
    }

    if !session.authenticated() {
        // try to connect using any public key stored in SSH-agent
        let _ = try_ssh_auth_with_agent(&session, &options.username);
    }

    if !session.authenticated() {
        // try with a password
        let password =
            rpassword::prompt_password(format!("Password for {}@{}: ", options.username, address))?;
        session.userauth_password(&options.username, &password)?;
    }

    if !session.authenticated() {
        anyhow::bail!("Could not connect to SSH of remote end");
    }

    // create temporary file
    let remote_filename = {
        let mut channel = session.channel_session()?;
        channel.exec("mktemp --tmpdir deltasync.XXXXXXXX")?;
        let mut s = String::new();
        channel.read_to_string(&mut s).unwrap();
        channel.wait_close()?;
        s
    };
    let remote_filename = remote_filename.trim();

    // open current executable as file
    let mut local_file = {
        let local_binary_path: std::path::PathBuf = std::env::current_exe()?;
        std::fs::File::open(local_binary_path)?
    };

    // write program to remote end
    {
        let mut channel = session.channel_session()?;
        let command = "cat >".to_string() + remote_filename;
        channel.exec(&command)?;
        let mut buf = vec![0; 1048576];
        loop {
            let n = local_file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            channel.write_all(&buf[0..n])?;
        }
        channel.send_eof()?;
        channel.wait_eof()?;
        channel.wait_close()?;
    }
    {
        let mut channel = session.channel_session()?;
        let command = "chmod +x ".to_string() + remote_filename;
        channel.exec(&command)?;
        channel.wait_eof()?;
        channel.wait_close()?;
    }

    // execute server in one-shot ephemeral mode with a random secret
    let secret = rand::distributions::Alphanumeric.sample_string(&mut rand::thread_rng(), 30);
    {
        let mut channel = session.channel_session()?;
        let command = remote_filename.to_string()
            + " --server -d -e -o --port \""
            + &options.server_port.to_string()
            + "\" --secret \""
            + &secret
            + "\" -w "
            + &options.workers.to_string();
        channel.exec(&command)?;
        channel.wait_eof()?;
        channel.wait_close()?;
    }

    Ok(secret)
}
