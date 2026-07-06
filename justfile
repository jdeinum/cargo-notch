
deploy: 
    cargo run notch pr --token "$(doppler run -p github_token -c prod -- printenv GITHUB_TOKEN)"
