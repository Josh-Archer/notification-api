Push-Location "C:\Code\home\notification-api"

# Build and tag Docker image
docker buildx build --platform linux/amd64,linux/arm64 -t jarcher1200/notification-api:latest --push .

Pop-Location
