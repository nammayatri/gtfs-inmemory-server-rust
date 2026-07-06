pipeline {
  agent {
    kubernetes {
      label 'dind-agent'
    }
  }
  environment {
    ACCOUNT_ID = '463356420488'
    ACCOUNT_ID2 = '147728078333'
    DOCKER_HUB_USERNAME = '12349901'
    IMAGE_NAME = 'gtfs-routes-service-rust'
  }
  stages {
    stage('Initialize') {
      steps {
        script {
          env.LAST_COMMIT_HASH = sh(script: "git rev-parse HEAD", returnStdout: true).trim().substring(0,6)
        }
      }
    }

    stage('Build and Push to Registries') {
      steps {
          // Build the Docker image for the Rust application
          sh "docker build -t ${env.IMAGE_NAME} ."

          // Login, Tag, and Push to the first ECR repository
          sh "aws ecr get-login-password --region ap-south-1 | docker login --username AWS --password-stdin ${env.ACCOUNT_ID}.dkr.ecr.ap-south-1.amazonaws.com"
          sh "docker tag ${env.IMAGE_NAME}:latest ${env.ACCOUNT_ID}.dkr.ecr.ap-south-1.amazonaws.com/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"
          sh "docker push ${env.ACCOUNT_ID}.dkr.ecr.ap-south-1.amazonaws.com/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"

          // Login, Tag, and Push to the second ECR repository
          sh "aws ecr get-login-password --region ap-south-1 | docker login --username AWS --password-stdin ${env.ACCOUNT_ID2}.dkr.ecr.ap-south-1.amazonaws.com"
          sh "docker tag ${env.IMAGE_NAME}:latest ${env.ACCOUNT_ID2}.dkr.ecr.ap-south-1.amazonaws.com/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"
          sh "docker push ${env.ACCOUNT_ID2}.dkr.ecr.ap-south-1.amazonaws.com/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"

          // Push to GCP Artifact Registry — master (ny-sandbox)
          withCredentials([file(credentialsId: 'gcp-sa-key', variable: 'GCP_KEY_FILE')]) {
            sh 'cat $GCP_KEY_FILE | docker login -u _json_key --password-stdin https://asia-south1-docker.pkg.dev'
            sh "docker tag ${env.IMAGE_NAME}:latest asia-south1-docker.pkg.dev/ny-sandbox/${env.IMAGE_NAME}/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"
            sh "docker push asia-south1-docker.pkg.dev/ny-sandbox/${env.IMAGE_NAME}/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"
          }

          // Push to GCP Artifact Registry — prod (ny-prod)
          withCredentials([file(credentialsId: 'gcp-sa-key-prod', variable: 'GCP_KEY_FILE_PROD')]) {
            sh 'cat $GCP_KEY_FILE_PROD | docker login -u _json_key --password-stdin https://asia-south1-docker.pkg.dev'
            sh "docker tag ${env.IMAGE_NAME}:latest asia-south1-docker.pkg.dev/ny-prod/${env.IMAGE_NAME}/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"
            sh "docker push asia-south1-docker.pkg.dev/ny-prod/${env.IMAGE_NAME}/${env.IMAGE_NAME}:${env.LAST_COMMIT_HASH}"
          }
      }
    }
  }
}
