use openssh::{KnownHosts, OwningCommand, Session, SessionBuilder, Stdio};
use openssh_sftp_client::metadata::Permissions;
use openssh_sftp_client::Sftp;
use serde::Deserialize;
use serde_json::from_slice;
use std::io::Write;
use std::{env::current_dir, fs::read, fs::File, path::Path, process::Command};
use zip::{write::SimpleFileOptions, ZipWriter};

#[derive(Debug, Deserialize)]
struct App {
    name: String,
    host_path: String,
    build_output_file: String,
    build_command: String,
    artifacts: Vec<String>,
    entrypoint: Option<String>,
}

impl App {
    pub fn build_output_file(&self) -> &String {
        &self.build_output_file
    }
    pub fn host_path(&self) -> &String {
        &self.host_path
    }
    pub fn name(&self) -> &String {
        &self.name
    }
    pub fn artifacts(&self) -> &Vec<String> {
        &self.artifacts
    }
    pub fn entrypoint(&self) -> &Option<String> {
        &self.entrypoint
    }
}

#[derive(Debug, Deserialize)]
struct Host {
    // name: String,
    key_path: String,
    user: String,
    host: String,
    port: u16,
}

impl Host {
    pub fn to_url(&self) -> String {
        format!("ssh://{}@{}:{}", self.user, self.host, self.port)
    }
}

#[tokio::main]
async fn main() {
    let hosts_config_path = format!("{}/ec2-deploy/hosts.json", env!("HOME"));

    let config_file = read(hosts_config_path).expect("Error opening hosts config file");
    let hosts: Vec<Host> =
        from_slice(config_file.as_slice()).expect("Error parsing hosts config file");

    // TODO: from cli
    let first_host = hosts.first().expect("No hosts found on config file");

    let session = SessionBuilder::default()
        .keyfile(Path::new(&first_host.key_path))
        .known_hosts_check(KnownHosts::Strict)
        .connect(first_host.to_url())
        .await
        .expect("Error");

    let mut child = session
        .subsystem("sftp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .await
        .expect("Enable to launch SFTP subsystem");

    let sftp = Sftp::new(
        child.stdin().take().unwrap(),
        child.stdout().take().unwrap(),
        Default::default(),
    )
    .await
    .expect("Error starting SFTP client");

    let cwd = current_dir().unwrap();
    let deploy_file = read(format!("{}/deploy.json", cwd.to_str().unwrap()))
        .expect("Error opening `deploy.json` file on current directory");

    let app: App = from_slice(deploy_file.as_slice()).expect("Error parsing `deploy.json` file");

    let build_output_file_path = Path::new(app.build_output_file().as_str());
    let host_output_path = format!(
        "{}/{}",
        app.host_path(),
        build_output_file_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    );

    println!("Deploying app: {}", app.name());

    let mut build_command: Vec<&str> = app.build_command.split_whitespace().collect();
    match Command::new(build_command.remove(0))
        .args(build_command)
        .output()
    {
        Ok(out) => {
            if out.stderr.len() > 0 {
                panic!(
                    "Got build error:\n{}",
                    String::from_utf8(out.stderr.to_vec()).unwrap()
                );
            }
            println!("Build ran successfully!");
        }
        Err(err) => {
            panic!("Error running build command:\n{}", err);
        }
    }

    let build_file = File::create(app.build_output_file())
        .expect(format!("Error creating output file `{}`", app.build_output_file()).as_str());

    let mut zip_build = ZipWriter::new(&build_file);

    for path_str in app.artifacts() {
        let path = Path::new(path_str.as_str());
        let name = path
            .file_name()
            .expect(format!("Error getting artifact path `{}`", path_str).as_str())
            .to_str()
            .unwrap();

        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

        if path.is_file() {
            zip_build
                .start_file(name, options)
                .expect(format!("Error including artifact `{}`", &path_str).as_str());

            let content = read(path_str.as_str())
                .expect(format!("Error reading artifact content `{}`", &path_str).as_str());

            zip_build
                .write_all(&content)
                .expect(format!("Error writing artifact content `{}`", path_str).as_str());
        }

        if path.is_dir() {
            for e in path.read_dir().unwrap().into_iter() {
                let entry = e.expect("Error reading artifact dir entry");

                zip_build
                    .start_file(entry.file_name().into_string().unwrap(), options)
                    .expect(format!("Error including artifact `{}`", &path_str).as_str());

                let content = read(entry.path()).expect(
                    format!(
                        "Error reading artifact content `{}`",
                        entry.path().to_str().unwrap()
                    )
                    .as_str(),
                );

                zip_build
                    .write_all(&content)
                    .expect(format!("Error writing artifact content `{}`", path_str).as_str());
            }
        }
    }

    zip_build
        .finish()
        .expect("Error writing to build output file");

    unwrap_command_stderr(session.command("mkdir").args(&["-p", app.host_path()]))
        .await
        .expect("Error creating app host directory");

    let mut fs = sftp.fs();

    fs.write(
        &host_output_path,
        read(app.build_output_file()).expect("Error reading new build file content"),
    )
    .await
    .expect("Error writing build file into host's fs");

    println!("Build output file written! Unzipping...");

    unwrap_command_stderr(
        session
            .command("unzip")
            .args(&["-o", &host_output_path.as_str()])
            .args(&["-d", app.host_path()]),
    )
    .await
    .expect("Error unzipping output file");

    if app.entrypoint().is_some() {
        let entrypoint = app.entrypoint().as_ref().unwrap();
        println!("Found entrypoint file `{}`", entrypoint);

        let host_entrypoint_path = format!("{}/{}", app.host_path(), entrypoint);
        if !app.artifacts().into_iter().any(|a| a.eq(entrypoint)) {
            println!("Entrypoint not fount on artifacts, uploading...");
            fs.write(
                &host_entrypoint_path,
                read(entrypoint).expect("Error reading entrypoint file"),
            )
            .await
            .expect("Error writing entrypoint file into host's fs")
        }

        fs.set_permissions(
            &host_entrypoint_path,
            Permissions::new()
                .set_execute_by_group(true)
                .set_execute_by_owner(true)
                .clone(),
        )
        .await
        .expect("Error giving entrypoint file execute permissions");
    } else {
        session
            .command("cd")
            .raw_args(&[app.host_path(), "&&"])
            .args(&["COMPOSE_STATUS_STDOUT=1", "docker-compose", "build"])
            .output()
            .await
            .expect("Error running `docker-compose build` command");

        session
            .command("cd")
            .raw_args(&[app.host_path(), "&&"])
            .args(&["docker-compose", "up", "-d"])
            .output()
            .await
            .expect("Error running `docker-compose up -d` command");

        println!("Stack built successfully!");
    }

    drop(fs);

    let (_, _) = futures::join!(session.close(), sftp.close());

    println!("Connection closed!")
}

async fn unwrap_command_stderr(command: &mut OwningCommand<&'_ Session>) -> Result<String, String> {
    match command.output().await {
        Ok(out) => {
            if out.stderr.len() > 0 {
                return Err(
                    format!("{}", String::from_utf8(out.stderr.to_vec()).unwrap()).to_string(),
                );
            }
            Ok(String::from_utf8(out.stdout).unwrap())
        }
        Err(err) => {
            panic!("Error running command:\n{}", err);
        }
    }
}
