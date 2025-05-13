use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bip39;
use clap::{Parser, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use indicatif::{ProgressBar, ProgressStyle};
use once_cell::sync::Lazy;
use pbkdf2::pbkdf2_hmac_array;
use rand::{rngs::OsRng, RngCore};
use rpassword::prompt_password;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Write; // Added for writing to file
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::str::FromStr;

const SALT_LENGTH: usize = 32;
const NONCE_LENGTH: usize = 12;
const KEY_LENGTH: usize = 32;
const ITERATIONS: u32 = 100_000;

// Add a static variable to store the password and key store hash
static PASSWORD_CACHE: Lazy<Mutex<Option<(String, String)>>> = Lazy::new(|| Mutex::new(None));

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// API endpoint URL (default: http://localhost:3000)
    #[arg(short, long, default_value = "http://localhost:3000")]
    endpoint: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Generate a new key pair
    GenerateKey {
        /// Name for the key pair
        #[arg(short, long)]
        name: String,
    },
    /// List all saved key pairs
    ListKeys,
    /// Export mnemonic for a key pair
    ExportKey {
        /// Name of the key pair to export
        #[arg(short, long)]
        name: String,
    },
    /// Import a key pair from a mnemonic phrase
    ImportKey {
        /// Name for the imported key pair
        #[arg(short, long)]
        name: String,
    },
    /// Generate multiple key pairs without passwords and output public keys
    BatchGen {
        /// Number of keys to generate
        #[arg(short, long, value_name = "COUNT")]
        count: u32,
    },
    /// Send a proof and ELF file to the server
    Send {
        /// Path to the proof file
        #[arg(short, long)]
        proof_file: PathBuf,

        /// Path to the ELF file
        #[arg(short = 'l', long)]
        elf_file: PathBuf,

        /// Name of the key pair to use for signing
        #[arg(short, long)]
        key_name: String,

        /// Proving system to use (default: sp1)
        #[arg(short = 's', long, default_value = "sp1")]
        proving_system: ProvingSystem,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ProvingSystem {
    Sp1,
    Circom,
    Risc0,
    Starknet,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyPair {
    public_key: Vec<u8>,
    public_key_string: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_secret_key: Option<EncryptedSecretKey>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedSecretKey {
    salt: Vec<u8>,
    nonce: Vec<u8>,
    encrypted_data: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct KeyStore {
    keys: HashMap<String, KeyPair>,
}

fn derive_key(password: &str, salt: &[u8]) -> [u8; KEY_LENGTH] {
    pbkdf2_hmac_array::<Sha256, KEY_LENGTH>(password.as_bytes(), salt, ITERATIONS)
}

fn encrypt_secret_key(secret_key: &[u8], password: &str) -> Result<EncryptedSecretKey> {
    let mut rng = OsRng;
    let mut salt = [0u8; SALT_LENGTH];
    let mut nonce = [0u8; NONCE_LENGTH];
    rng.fill_bytes(&mut salt);
    rng.fill_bytes(&mut nonce);

    let key_bytes = derive_key(password, &salt);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);

    let encrypted_data = cipher
        .encrypt(Nonce::from_slice(&nonce), secret_key)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    Ok(EncryptedSecretKey {
        salt: salt.to_vec(),
        nonce: nonce.to_vec(),
        encrypted_data,
    })
}

fn decrypt_secret_key(encrypted: &EncryptedSecretKey, password: &str) -> Result<Vec<u8>> {
    let key_bytes = derive_key(password, &encrypted.salt);
    let key = Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);

    cipher
        .decrypt(
            Nonce::from_slice(&encrypted.nonce),
            encrypted.encrypted_data.as_slice(),
        )
        .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))
}

fn create_progress_bar(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    pb.set_message(message.to_string());
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}

fn load_key_store() -> Result<KeyStore> {
    let key_store_path = PathBuf::from("key_store.json");
    if key_store_path.exists() {
        let contents = fs::read_to_string(&key_store_path)?;
        let key_store: KeyStore = serde_json::from_str(&contents)?;
        Ok(key_store)
    } else {
        Ok(KeyStore {
            keys: HashMap::new(),
        })
    }
}

fn save_key_store(key_store: &KeyStore) -> Result<()> {
    let key_store_path = PathBuf::from("key_store.json");
    let contents = serde_json::to_string_pretty(key_store)?;
    fs::write(key_store_path, contents)?;
    Ok(())
}

fn generate_key_pair(name: &str) -> Result<()> {
    let mut key_store = load_key_store()?;

    if key_store.keys.contains_key(name) {
        anyhow::bail!("Key pair with name '{}' already exists", name);
    }

    // Generate a new key pair
    let mut rng = OsRng;
    let signing_key = SigningKey::generate(&mut rng);
    let verifying_key = signing_key.verifying_key();
    let public_key_bytes = verifying_key.to_bytes();
    let public_key_string = BASE64.encode(&public_key_bytes);

    // Generate mnemonic from secret key
    let secret_key_bytes = signing_key.to_bytes();
    let mnemonic = bip39::Mnemonic::from_entropy(&secret_key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to generate mnemonic: {}", e))?;
    let mnemonic_string = mnemonic.to_string();

    println!("\n📝 IMPORTANT: Save this mnemonic phrase securely for your testnet participation!");
    println!("⚠️  WARNING: This is the only time you'll see this mnemonic! You'll need it to recover your secret key if the key store is lost!\n");
    println!("{}", mnemonic_string);

    // Get password for secret key encryption
    let password = prompt_password("\nEnter password for secret key: ")
        .map_err(|e| anyhow::anyhow!("Failed to read password: {}", e))?;
    let confirm_password = prompt_password("Confirm password: ")
        .map_err(|e| anyhow::anyhow!("Failed to read password: {}", e))?;

    if password != confirm_password {
        anyhow::bail!("Passwords do not match");
    }

    // Encrypt the secret key
    let encrypted_secret = encrypt_secret_key(&secret_key_bytes, &password)?;

    // Save the key pair
    key_store.keys.insert(
        name.to_string(),
        KeyPair {
            public_key: public_key_bytes.to_vec(),
            public_key_string: public_key_string.clone(),
            encrypted_secret_key: Some(encrypted_secret),
        },
    );

    save_key_store(&key_store)?;
    println!("\n✅ Generated new key pair '{}'", name);
    println!("🔑 Public key: {}", public_key_string);
    Ok(())
}

fn batch_gen_keys(count: u32) -> Result<()> {
    if count == 0 {
        println!("Number of keys to generate must be greater than 0.");
        return Ok(());
    }

    let mut key_store = load_key_store()?;
    let mut public_keys_to_write = Vec::new();

    println!("Generating {} key pair(s)...", count);
    let pb = ProgressBar::new(count as u64);
    let pb_style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_bar()) // Fallback style
        .progress_chars("=> ");
    pb.set_style(pb_style.clone());
    pb.set_message("Generating keys");


    let mut next_key_idx_base = 0;
    // Find a starting index for batch_key_X that doesn't exist yet.
    // This helps avoid immediate collisions if batch-gen is run multiple times.
    loop {
        let potential_name = format!("batch_key_{}", next_key_idx_base);
        if !key_store.keys.contains_key(&potential_name) {
            break;
        }
        next_key_idx_base += 1;
    }

    for i in 0..count {
        let mut current_name_candidate_idx = next_key_idx_base + i;
        let mut final_key_name;
        // Ensure unique name even if there are gaps or manual additions matching the pattern
        loop {
            final_key_name = format!("batch_key_{}", current_name_candidate_idx);
            if !key_store.keys.contains_key(&final_key_name) {
                break;
            }
            current_name_candidate_idx += 1; // Increment and try next index
        }
        
        // Generate a new key pair
        let mut rng = OsRng;
        let signing_key = SigningKey::generate(&mut rng); // Secret key is generated here
        let verifying_key = signing_key.verifying_key();
        let public_key_bytes = verifying_key.to_bytes();
        let public_key_string = BASE64.encode(&public_key_bytes);

        // Encrypt secret key with empty password
        let secret_key_bytes = signing_key.to_bytes();
        let empty_password = ""; // 使用空密码
        let encrypted_secret = encrypt_secret_key(&secret_key_bytes, empty_password)
            .expect("Failed to encrypt secret key with empty password");
        
        // Store the key pair with an encrypted secret key (using empty password)
        key_store.keys.insert(
            final_key_name.clone(),
            KeyPair {
                public_key: public_key_bytes.to_vec(),
                public_key_string: public_key_string.clone(),
                encrypted_secret_key: Some(encrypted_secret), // 使用空密码加密
            },
        );
        public_keys_to_write.push(public_key_string);
        pb.inc(1);
    }

    pb.finish_with_message(format!("✅ Generated {} key pair(s)", count));

    // Save the updated key store
    save_key_store(&key_store)?;
    println!("💾 Key store updated with new public keys.");

    // Write public keys to public_keys.txt
    let output_file_path = PathBuf::from("public_keys.txt");
    let mut file = fs::File::create(&output_file_path)
        .with_context(|| format!("Failed to create file: {}", output_file_path.display()))?;
    
    for pub_key_str in public_keys_to_write {
        writeln!(file, "{}", pub_key_str)
            .with_context(|| format!("Failed to write to file: {}", output_file_path.display()))?;
    }
    println!("🔑 All public keys written to {}", output_file_path.display());

    Ok(())
}


fn list_keys() -> Result<()> {
    let key_store = load_key_store()?;

    if key_store.keys.is_empty() {
        println!("No key pairs found. Generate one with 'generate-key' command.");
        return Ok(());
    }

    println!("Available key pairs:");
    for (name, key_pair) in key_store.keys {
        let secret_status = if key_pair.encrypted_secret_key.is_some() {
            "(secret encrypted)"
        } else {
            "(secret not stored/encrypted)"
        };
        println!("- {} (Public key: {}) {}", name, key_pair.public_key_string, secret_status);
    }
    Ok(())
}

// Calculate hash of key store contents
fn calculate_key_store_hash(key_store: &KeyStore) -> String {
    let serialized = serde_json::to_string(key_store).unwrap_or_default();
    format!("{:x}", Sha256::digest(serialized.as_bytes()))
}

fn sign_payload(payload: &[u8], key_name: &str) -> Result<Vec<u8>> {
    let key_store = load_key_store()?;
    let key_store_hash = calculate_key_store_hash(&key_store);

    let key_pair = key_store
        .keys
        .get(key_name)
        .ok_or_else(|| anyhow::anyhow!("Key pair '{}' not found", key_name))?;

    let encrypted_secret = key_pair
        .encrypted_secret_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Secret key not found or not encrypted for '{}'. Keys generated with 'batch-gen' do not store encrypted secrets and cannot be used for signing directly with this method.", key_name))?;

    // Create a new scope for the password guard to ensure it's dropped properly
    let password = {
        let mut password_guard = PASSWORD_CACHE.lock().unwrap();

        if let Some((stored_password, stored_hash)) = password_guard.as_ref() {
            // Check if key store has changed
            if stored_hash != &key_store_hash {
                *password_guard = None;
                drop(password_guard);
                return sign_payload(payload, key_name);
            }
            stored_password.clone()
        } else {
            // If no password is stored, prompt for it
            let new_password = prompt_password("Enter password to decrypt the secret key: ")
                .map_err(|e| anyhow::anyhow!("Failed to read password: {}", e))?;

            // Try to decrypt with the password to verify it's correct
            if let Err(e) = decrypt_secret_key(encrypted_secret, &new_password) {
                anyhow::bail!("Invalid password: {}", e);
            }

            // Store the password and key store hash
            *password_guard = Some((new_password.clone(), key_store_hash));
            new_password
        }
    }; // password_guard is dropped here

    // Only show the progress bar after we have the password
    let pb = create_progress_bar("✍️  Signing payload...");

    let secret_key_bytes = decrypt_secret_key(encrypted_secret, &password)?;
    let secret_key_array: [u8; 32] = secret_key_bytes.clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid secret key length"))?;

    let signing_key = SigningKey::from_bytes(&secret_key_array);
    let signature = signing_key.sign(payload);
    pb.finish_with_message("✍️  Payload signed successfully");

    Ok(signature.to_bytes().to_vec())
}

fn get_public_key(key_name: &str) -> Result<Vec<u8>> {
    let key_store = load_key_store()?;
    let key_pair = key_store
        .keys
        .get(key_name)
        .ok_or_else(|| anyhow::anyhow!("Key pair '{}' not found", key_name))?;
    Ok(key_pair.public_key.clone())
}

fn export_key(name: &str) -> Result<()> {
    let key_store = load_key_store()?;
    let key_pair = key_store
        .keys
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("Key pair '{}' not found", name))?;

    let encrypted_secret = key_pair
        .encrypted_secret_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Secret key not found or not encrypted for '{}'. Cannot export mnemonic.", name))?;

    // Prompt for password
    let password = prompt_password("Enter password to decrypt the secret key: ")
        .map_err(|e| anyhow::anyhow!("Failed to read password: {}", e))?;

    // Decrypt the secret key with better error handling
    let secret_key_bytes = match decrypt_secret_key(encrypted_secret, &password) {
        Ok(bytes) => bytes,
        Err(_) => {
            println!("\n❌ Error: Invalid password. Please try again with the correct password.");
            return Ok(());
        }
    };

    // Generate mnemonic from secret key
    let mnemonic = bip39::Mnemonic::from_entropy(&secret_key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to generate mnemonic: {}", e))?;
    let mnemonic_string = mnemonic.to_string();

    println!("\n🔑 Mnemonic for key pair '{}':\n", name);
    println!("{}", mnemonic_string);
    println!("\n⚠️  WARNING: Keep this mnemonic secure and never share it with anyone!");
    Ok(())
}

fn import_key(name: &str) -> Result<()> {
    let mut key_store = load_key_store()?;

    if key_store.keys.contains_key(name) {
        anyhow::bail!("Key pair with name '{}' already exists", name);
    }

    // Prompt for mnemonic
    println!("\nEnter your mnemonic phrase (12 or 24 words):");
    let mut mnemonic_input = String::new();
    std::io::stdin().read_line(&mut mnemonic_input)?;
    let mnemonic_input = mnemonic_input.trim();

    // Parse mnemonic
    let mnemonic = bip39::Mnemonic::from_str(mnemonic_input)
        .map_err(|e| anyhow::anyhow!("Invalid mnemonic phrase: {}", e))?;

    // Convert mnemonic to secret key
    let secret_key_bytes = mnemonic.to_entropy();
    let secret_key_array: [u8; 32] = secret_key_bytes.clone()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid secret key length"))?;

    // Create signing key and get public key
    let signing_key = SigningKey::from_bytes(&secret_key_array);
    let verifying_key = signing_key.verifying_key();
    let public_key_bytes = verifying_key.to_bytes();
    let public_key_string = BASE64.encode(&public_key_bytes);

    // Get password for secret key encryption
    let password = prompt_password("\nEnter password to encrypt the secret key: ")
        .map_err(|e| anyhow::anyhow!("Failed to read password: {}", e))?;
    let confirm_password = prompt_password("Confirm password: ")
        .map_err(|e| anyhow::anyhow!("Failed to read password: {}", e))?;

    if password != confirm_password {
        anyhow::bail!("Passwords do not match");
    }

    // Encrypt the secret key
    let encrypted_secret = encrypt_secret_key(&secret_key_bytes, &password)?;

    // Save the key pair
    key_store.keys.insert(
        name.to_string(),
        KeyPair {
            public_key: public_key_bytes.to_vec(),
            public_key_string: public_key_string.clone(),
            encrypted_secret_key: Some(encrypted_secret),
        },
    );

    save_key_store(&key_store)?;
    println!("\n✅ Successfully imported key pair '{}'", name);
    println!("🔑 Public key: {}", public_key_string);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = reqwest::Client::new();

    match args.command {
        Commands::GenerateKey { name } => {
            generate_key_pair(&name)?;
        }
        Commands::ListKeys => {
            list_keys()?;
        }
        Commands::ExportKey { name } => {
            export_key(&name)?;
        }
        Commands::ImportKey { name } => {
            import_key(&name)?;
        }
        Commands::BatchGen { count } => {
            batch_gen_keys(count)?;
        }
        Commands::Send {
            proof_file,
            elf_file,
            key_name,
            proving_system,
        } => {
            // Create progress bars
            let reading_pb = create_progress_bar("📂 Reading files...");

            // Read the files as binary data
            let proof_content = fs::read(&proof_file)
                .with_context(|| format!("Failed to read proof file: {}", proof_file.display()))?;

            let elf_content = fs::read(&elf_file)
                .with_context(|| format!("Failed to read ELF file: {}", elf_file.display()))?;

            reading_pb.finish_with_message("📂 Files read successfully");

            // Create the request body with canonical string
            let request_body = serde_json::json!({
                "proof": BASE64.encode(&proof_content),
                "elf": BASE64.encode(&elf_content),
                "proof_filename": proof_file.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown"),
                "elf_filename": elf_file.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown"),
                "proving_system": format!("{:?}", proving_system).to_lowercase(),
                "canonical_string": format!(
                    "proof:{}\nelf:{}\nproof_filename:{}\nelf_filename:{}\nproving_system:{}",
                    BASE64.encode(&proof_content),
                    BASE64.encode(&elf_content),
                    proof_file.file_name().and_then(|n| n.to_str()).unwrap_or("unknown"),
                    elf_file.file_name().and_then(|n| n.to_str()).unwrap_or("unknown"),
                    format!("{:?}", proving_system).to_lowercase()
                )
            });

            // Sign the canonical string
            let canonical_string = request_body["canonical_string"].as_str().unwrap();
            let signature = sign_payload(canonical_string.as_bytes(), &key_name)?;
            let public_key = get_public_key(&key_name)?;

            // Send the request
            let sending_pb = create_progress_bar("🚀 Sending to server...");
            let response = client
                .post(format!("{}/api/proof", args.endpoint))
                .header("Content-Type", "application/json")
                .header("X-Signature", BASE64.encode(&signature))
                .header("X-Public-Key", BASE64.encode(&public_key))
                .json(&request_body)
                .send()
                .await
                .with_context(|| format!("Failed to send request to {}", args.endpoint))?;

            sending_pb.finish_with_message("🚀 Request sent successfully");

            // Check if the request was successful
            if response.status().is_success() {
                println!("\n✅ Successfully sent files to {}", args.endpoint);
                let response_text = response.text().await?;
                println!("Server response: {}", response_text);
            } else {
                println!("\n❌ Error: Server returned status {}", response.status());
                let error_text = response.text().await?;
                println!("Error details: {}", error_text);
            }
        }
    }

    Ok(())
}
