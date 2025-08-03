Push-Location "C:\Code\home\notification-api"

# Build and tag Docker image
docker build -t jarcher1200/notification-api:latest .

# Push to Docker Hub
docker push jarcher1200/notification-api:latest

Pop-Location
