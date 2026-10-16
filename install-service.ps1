# Installs NymFlickrCast as a Windows service that starts with Windows, and allows it through the firewall.
# Run as administrator from the folder with nym_flickr_cast.exe and secrets\ (the unzipped release).
# The service runs the exe from that folder, so put it somewhere permanent first, e.g. C:\Program Files\NymFlickrCast.
#   .\install-service.ps1 [-Port 8080]    install (or reinstall) and start
#   .\install-service.ps1 -Uninstall      stop and remove
param([int]$Port = 8080, [switch]$Uninstall)
$ErrorActionPreference = 'Stop'
$name = 'NymFlickrCast'
$exe = Join-Path $PSScriptRoot 'nym_flickr_cast.exe'

$admin = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $admin.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script as administrator.'
}

# Remove any earlier install, so reinstalling picks up a new port or folder.
if (Get-Service $name -ErrorAction SilentlyContinue) {
    Stop-Service $name -ErrorAction SilentlyContinue
    sc.exe delete $name | Out-Null
}
Remove-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue
if ($Uninstall) {
    Write-Host "Removed the $name service."
    return
}

if (-not (Test-Path $exe)) { throw "nym_flickr_cast.exe not found next to this script ($PSScriptRoot)." }
New-Service -Name $name -BinaryPathName "`"$exe`" --service --port $Port" -DisplayName $name `
    -Description 'Browse Flickr albums and cast them to Google Cast devices' -StartupType Automatic | Out-Null
sc.exe config $name start= delayed-auto | Out-Null # start once the network is up
sc.exe failure $name reset= 86400 actions= restart/5000/restart/5000/restart/60000 | Out-Null
# Phones and iPads open the web app, and Cast devices fetch the photos, from other machines on the network.
New-NetFirewallRule -DisplayName $name -Direction Inbound -Program $exe -Action Allow -Profile Private, Domain | Out-Null
Start-Service $name
Write-Host "Installed and started ${name}: http://localhost:$Port/"
