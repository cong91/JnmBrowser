@echo off
echo === Building auto-reg-live ===
cd /d C:\Users\mrc\Documents\projects\JnmBrowser
cargo build --manifest-path src-tauri/Cargo.toml --features auto-reg-live --bin auto-reg-live
if %ERRORLEVEL% NEQ 0 (
    echo BUILD FAILED
    pause
    exit /b 1
)
echo.
echo === Running CHROMIUM registration with CDK GMAIL-B974-YLTW-XT46-BSW4 ===
echo === Verified flow: email -> password -> OTP -> About You ===
cargo run --manifest-path src-tauri/Cargo.toml --features auto-reg-live --bin auto-reg-live -- --cdk=GMAIL-B974-YLTW-XT46-BSW4 --browser=chromium --network=none --accounts-per-cdk=1 --email-provider=gmail.123452026.xyz
echo.
echo === Done ===
pause
