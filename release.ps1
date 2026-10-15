# Builds a release and zips it with the secrets folder into dist\NymFlickrCast.zip.
# The zip contains your Flickr API key: share it only with people you'd give the key to.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = 'dist\NymFlickrCast'
$zip = 'dist\NymFlickrCast.zip'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory $stage | Out-Null

# The app looks for secrets\flickrkey.txt next to where it is started, so keep that layout.
Copy-Item 'target\release\nym_flickr_cast.exe', 'readme.md' $stage
Copy-Item 'secrets' $stage -Recurse

Compress-Archive -Path $stage -DestinationPath $zip -Force
Write-Host "Created $zip"
