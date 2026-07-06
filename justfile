
deploy: 
    cargo run pr --token "$(doppler run -p github_token -c prod -- printenv GITHUB_TOKEN)"
