
deploy:
    NOTCH__REPO__TOKEN="$(doppler run -p github_token -c prod -- printenv GITHUB_TOKEN)" cargo run notch pr
