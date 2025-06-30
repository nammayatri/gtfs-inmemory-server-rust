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
          withCredentials([string(credentialsId: 'pk_docker_hub', variable: 'DOCKER_PASSWORD')]) {
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
            
        }
      }
    }
  }
}
