$ErrorActionPreference = "Stop"

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$javascriptRoots = @(
    (Join-Path $repositoryRoot "src-tauri\scripts"),
    (Join-Path $repositoryRoot "web"),
    (Join-Path $repositoryRoot "tests\javascript")
)
$javascriptFiles = @(
    Get-ChildItem -LiteralPath $javascriptRoots -Filter "*.js" -File
)
if ($javascriptFiles.Count -eq 0) {
    throw "JavaScript files were not found."
}

node --version
foreach ($file in $javascriptFiles) {
    node --check $file.FullName
    if ($LASTEXITCODE -ne 0) {
        throw "JavaScript syntax check failed: $($file.FullName)"
    }
}

$testFiles = @(
    Get-ChildItem `
        -LiteralPath (Join-Path $repositoryRoot "tests\javascript") `
        -Filter "*.test.js" `
        -File |
        ForEach-Object { $_.FullName }
)
if ($testFiles.Count -eq 0) {
    throw "JavaScript test files were not found."
}

node --test @testFiles
if ($LASTEXITCODE -ne 0) {
    throw "JavaScript tests failed."
}
