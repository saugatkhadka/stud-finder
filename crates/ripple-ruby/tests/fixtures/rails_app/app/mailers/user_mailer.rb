class UserMailer < ActionMailer::Base
  def welcome
    mail(to: "x@example.com")
  end
end
