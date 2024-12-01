$programPath = "target/release/bili-dynamic-spider.exe"
while ($true) {
    Start-Process -FilePath $programPath -Wait -NoNewWindow
    Write-Output "Restart in 2 seconds"
    Start-Sleep -Seconds 2 
}
